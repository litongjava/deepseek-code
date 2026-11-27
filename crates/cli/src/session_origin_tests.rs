use super::*;
use clap::Parser;
use deepseek_code_core::services::memory_files::FileMemory;

#[test]
fn memory_turn_producers_keep_explicit_origin() -> Result<()> {
    let root = tempfile::tempdir()?;
    let paths = RuntimePaths::new(root.path().join("data"))?;
    let context = deepseek_code_core::services::ServiceContext::open(
        paths,
        Arc::new(deepseek_code_core::services::EnvironmentCredentials),
    )?;
    let sessions = deepseek_code_core::runtime::SessionStore::new(context.db.clone());
    let id = sessions.create(root.path(), "model", "provider")?;
    let shard = context.db.open_session(&id)?;
    let positional = Cli::try_parse_from(["dcl", "--print"])?;
    let child = Cli::try_parse_from([
        "dcl",
        "--resume",
        &id,
        "--generated-continuation=delegation",
    ])?;
    let expanded = Cli::try_parse_from(["dcl", "--resume", &id, "--original-user-input=/work"])?;
    let notification = json!("saffron");
    let typed = json!([{"type":"text","text":"saffron"}]);
    for (producer, origin, tag) in [
        (
            "CLI positional",
            TurnOrigin::command_line(&positional, "saffron"),
            "typed",
        ),
        (
            "background Agent and fork",
            TurnOrigin::command_line(&child, "saffron"),
            "delegation",
        ),
        (
            "REPL and speech",
            TurnOrigin::User("saffron"),
            "typed",
        ),
        (
            "SDK initialPrompt prefix",
            TurnOrigin::User("hello"),
            "typed",
        ),
        (
            "inline skill and file mention",
            TurnOrigin::User("/work"),
            "typed",
        ),
        (
            "expanded worker handoff",
            TurnOrigin::command_line(&expanded, "saffron"),
            "typed",
        ),
        (
            "MCP server expansion",
            TurnOrigin::McpPrompt,
            "mcp_prompt",
        ),
        (
            "queued user retry",
            TurnOrigin::Queue("typed", &typed),
            "typed",
        ),
        (
            "queued notification retry",
            TurnOrigin::Queue("task_notification", &notification),
            "task_notification",
        ),
        (
            "explicit continuation",
            TurnOrigin::Continuation,
            "continuation",
        ),
        (
            "goal continuation",
            TurnOrigin::GoalContinuation,
            "goal_continuation",
        ),
        (
            "error resume",
            TurnOrigin::ErrorResume,
            "error_resume",
        ),
        (
            "cron and loop wakeups",
            TurnOrigin::Scheduled,
            "scheduled",
        ),
        ("session start hook", TurnOrigin::InitialHook, "hook"),
    ] {
        let message = origin.admit(&shard, "saffron", None)?;
        assert_eq!(
            shard.read(|r| r.message_metadata(&message))?.unwrap()["promptOrigin"],
            tag,
            "{producer}"
        );
    }
    assert_eq!(TurnOrigin::HookWake.tag(), "hook");
    Ok(())
}

#[tokio::test]
async fn memory_queued_notification_stays_generated_after_overflow() -> Result<()> {
    queued_compaction(false).await
}

#[tokio::test]
async fn memory_autocompaction_protects_queued_human_input() -> Result<()> {
    queued_compaction(true).await
}

async fn queued_compaction(autocompact: bool) -> Result<()> {
    use axum::{response::IntoResponse, Json};
    let root = tempfile::tempdir()?;
    let paths = RuntimePaths::new(root.path().join("data"))?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}", listener.local_addr()?);
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let capture = requests.clone();
    use deepseek_code_core::db::session_store::{SessionDb, Stores};
    fn check_admitted(stores: &Stores, shard: &SessionDb) -> Result<()> {
        let (rows, bytes) = shard.repository().active_history_size()?;
        assert_eq!((rows, bytes.div_ceil(3)), (3, 48_514));
        let pending: bool = stores.shard_sql(shard.id(), |c| {
            c.query_row(
                "SELECT EXISTS(SELECT 1 FROM messages m JOIN peripheral_settings p
                 ON p.key='prompt.pending.'||m.id WHERE m.content='saffron QUEUED_INPUT_7F03')",
                [],
                |row| row.get(0),
            )
        })??;
        assert!(pending);
        Ok(())
    }
    let admitted = Arc::new(std::sync::OnceLock::<(Arc<Stores>, SessionDb)>::new());
    let summary_session = admitted.clone();
    let app = axum::Router::new().route("/v1/chat/completions", axum::routing::post(move |headers: axum::http::HeaderMap, Json(body): Json<Value>| {
        let capture = capture.clone();
        let summary_session = summary_session.clone();
        async move {
            if headers.get("x-dcl-request-kind").is_some_and(|kind| kind == "compaction") {
                if autocompact {
                    // The 24,000-token window triggers at 19,200. Observe the
                    // admitted input's pending marker while its prefix is being summarized.
                    let (stores, shard) = summary_session.get().unwrap();
                    check_admitted(stores, shard).unwrap();
                    assert!(!body["messages"].to_string().contains("QUEUED_INPUT_7F03"));
                }
                if body["stream"] != true {
                    return Json(json!({"id":"summary","object":"chat.completion","created":0,"model":"model","choices":[{"index":0,"message":{"role":"assistant","content":"compacted"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}})).into_response();
                }
            } else {
                let mut calls = capture.lock().unwrap();
                calls.push(body);
                if !autocompact && calls.len() == 1 {
                    return (axum::http::StatusCode::BAD_REQUEST, Json(json!({"error":{"code":"context_length_exceeded","message":"prompt is too long"}}))).into_response();
                }
            }
            ([("content-type", "text/event-stream")], "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"done\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n").into_response()
        }
    }));
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let cli = Cli::try_parse_from(["dcl", "--print", "--path", root.path().to_str().unwrap()])?;
    let provider = serde_json::from_value(
        json!({"id":"fixture","name":"Fixture","base_url":url,"api_key":"fixture-only","api_format":"openai","enabled":true,"models":[{"id":"model","name":"fixture","enabled":true,"max_tokens":4096,"context_window":if autocompact {24000} else {128000},"supports_vision":false,"supports_web_search":false}]}),
    )?;
    let session = Session::open(
        &cli,
        &paths,
        Selection {
            providers: vec![provider],
            provider_id: "fixture".into(),
            model: "model".into(),
        },
        None,
    )
    .await?;
    let shard = session.selected_session()?;
    assert!(admitted
        .set((session.store.db.clone(), shard.clone()))
        .is_ok());
    let memory = FileMemory::session(&session.hook_service_context(), shard.id())?;
    memory.put("saffron.md","---\nname: saffron\ndescription: saffron\nmetadata:\n  type: user\n---\nNOTIFICATION-MATCH\n", None)?;
    shard.write(|w| {
        w.append("user", "earlier question")?;
        w.append(
            "assistant",
            &if autocompact {
                "earlier answer ".repeat(9700)
            } else {
                "answer".into()
            },
        )
    })?;
    if autocompact {
        use deepseek_code_core::services::input_queue::{QueueMode, QueueOrigin};
        shard.enqueue(
            "typed-queue-probe",
            &json!("saffron QUEUED_INPUT_7F03"),
            QueueMode::NextTurn,
            QueueOrigin::User,
        )?;
    } else {
        shard.enqueue_cli_input(
            deepseek_code_core::services::input_queue::QueueOrigin::TaskNotification,
            "saffron",
        )?;
    }
    let item = session.store.db.next_desktop_input(shard.id())?.unwrap();
    let result = session.dispatch_input(&cli, item, &mut None).await;
    server.abort();
    let _ = server.await;
    result?;
    session.close().await?;
    let calls = requests.lock().unwrap();
    if autocompact {
        let rows = shard.repository().messages()?;
        let boundary = rows
            .iter()
            .rposition(|row| row.is_compact_boundary)
            .unwrap();
        assert_eq!(rows[boundary + 1].content, "saffron QUEUED_INPUT_7F03");
        assert_eq!(calls.len(), 1);
        let body = calls[0]["messages"].to_string();
        assert!(body.contains("earlier transcript retained"));
        assert!(body.contains("QUEUED_INPUT_7F03") && !body.contains("NOTIFICATION-MATCH"));
        return Ok(());
    }
    assert_eq!(calls.len(), 2);
    assert!(calls[1]["messages"].to_string().contains("saffron"));
    assert!(!calls[1]["messages"]
        .to_string()
        .contains("NOTIFICATION-MATCH"));
    let row = shard
        .repository()
        .messages()?
        .into_iter()
        .find(|m| m.content == "saffron")
        .unwrap();
    assert_eq!(
        shard.read(|r| r.message_metadata(&row.id))?.unwrap()["promptOrigin"],
        "task_notification"
    );
    Ok(())
}
