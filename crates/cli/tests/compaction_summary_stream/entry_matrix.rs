use super::*;
use axum::{
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
};

const BASH_OUTPUT: &str = r#"{"stdout":"I56_STDOUT","stderr":"I56_STDERR","exit_code":0}"#;
const CURRENT: &str = "I56_PROTECTED_INPUT";

#[derive(Clone, Copy, Debug, PartialEq)]
enum Entry {
    Manual,
    MidTurn,
    ProtectedPrefix,
    PreTurn,
}

#[derive(Clone, Copy, Debug)]
enum Case {
    Snapshot(Entry),
    RejectedAfterRecovery,
    Overflow { status: StatusCode, cooldown: bool },
}

#[derive(Default)]
struct Provider {
    case: Option<Case>,
    posts: Vec<(&'static str, Value)>,
    main: usize,
}

fn sse(frames: &[Value]) -> Response {
    let body: String = frames
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect();
    ([("content-type", "text/event-stream")], body).into_response()
}

async fn respond(
    State(state): State<Arc<Mutex<Provider>>>,
    uri: Uri,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    let mut state = state.lock().unwrap();
    let kind = if uri.path().ends_with("/compact") {
        "native"
    } else if headers
        .get("x-dcl-request-kind")
        .is_some_and(|kind| kind == "compaction")
    {
        "summary"
    } else {
        "main"
    };
    state.posts.push((kind, body["input"].clone()));
    let main_before = state.main;
    if kind == "main" {
        state.main += 1;
    }
    if matches!(state.case, Some(Case::RejectedAfterRecovery)) {
        if kind == "native" && main_before == 9 {
            return Json(json!({"object":"response.compaction","output":[{"type":"compaction","id":"cmp-nine","encrypted_content":"recovered-at-nine"}]})).into_response();
        }
        if kind == "summary" {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [("retry-after", "0")],
                "busy",
            )
                .into_response();
        }
        if kind == "main" && main_before == 9 {
            return (StatusCode::PAYLOAD_TOO_LARGE, "input rejected").into_response();
        }
    }
    if kind == "native" {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [("retry-after", "0")],
            "busy",
        )
            .into_response();
    }
    if let Some(Case::Overflow { status, cooldown }) = state.case {
        if kind == "summary" {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                [("retry-after", "0")],
                "busy",
            )
                .into_response();
        }
        if main_before == usize::from(cooldown) {
            if status == StatusCode::PAYLOAD_TOO_LARGE {
                return (status, "input rejected").into_response();
            }
            let error = json!({"code":"context_length_exceeded","message":"input rejected"});
            if status == StatusCode::OK {
                return sse(&[
                    json!({"type":"response.failed","response":{"status":"failed","error":error}}),
                ]);
            }
            return (status, Json(json!({"error":error}))).into_response();
        }
    }
    let tool = kind == "main"
        && ((matches!(state.case, Some(Case::RejectedAfterRecovery)) && main_before < 9)
            || (main_before == 0
                && matches!(
                    state.case,
                    Some(Case::Snapshot(Entry::MidTurn) | Case::Overflow { cooldown: true, .. })
                )));
    let mut frames = Vec::new();
    let output = if tool {
        json!([{"type":"function_call","id":format!("fc-{main_before}"),"call_id":format!("read-{main_before}"),"name":"Read","arguments":r#"{"file_path":"missing.txt"}"#}])
    } else {
        frames.push(json!({"type":"response.output_text.delta","delta":"fixture reply"}));
        json!([{"type":"message","role":"assistant","content":[{"type":"output_text","text":"fixture reply"}]}])
    };
    frames.push(json!({"type":"response.completed","response":{"id":"r-test","status":"completed","output":output,
        "usage":{"input_tokens":if tool {52000} else {150},"output_tokens":10}}}));
    sse(&frames)
}

// A whole Responses replay uses function_call_output. The summary planner can also
// place canonical model rows in a text fragment. Compare the decoded result in either form.
fn bash_output(input: &Value) -> Option<String> {
    for item in input.as_array()? {
        if item["type"] == "function_call_output" && item["call_id"] == "bashfixture" {
            return item["output"].as_str().map(String::from);
        }
        for block in item["content"].as_array().into_iter().flatten() {
            let Some(text) = block["text"].as_str() else {
                continue;
            };
            let Some((_, rows)) = text.split_once("Conversation JSON:\n") else {
                continue;
            };
            let Some(Ok(rows)) = serde_json::Deserializer::from_str(rows)
                .into_iter::<Vec<Value>>()
                .next()
            else {
                continue;
            };
            if let Some(row) = rows.iter().find(|r| r["tool_call_id"] == "bashfixture") {
                return row["content"].as_str().map(String::from);
            }
        }
    }
    None
}

async fn run_case(case: Case) {
    let state = Arc::new(Mutex::new(Provider::default()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let app = Router::new()
        .fallback(post(respond))
        .with_state(state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let temp = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let id = store.create(temp.path(), "gpt-fixture", "local").unwrap();
    write_responses_fixture(&paths, &base);
    let seed = resumed_query(&paths, temp.path(), &id, "Seed a completed turn").await;
    assert!(
        seed.status.success(),
        "{}",
        String::from_utf8_lossy(&seed.stderr)
    );

    if let Case::Snapshot(entry) = case {
        store
            .db
            .shard_sql(&id, |db| -> anyhow::Result<()> {
                use deepseek_code_core::db::message_repo::{
                    insert_tool_call, update_message_content,
                };
                let assistant = get_messages_by_conversation(db, &id)?
                    .into_iter()
                    .rev()
                    .find(|r| r.role == "assistant")
                    .unwrap();
                let size = match entry {
                    Entry::ProtectedPrefix => 130_950,
                    Entry::PreTurn => 155_000,
                    _ => 100,
                };
                update_message_content(db, &assistant.id, &"a".repeat(size))?;
                insert_tool_call(
                    db,
                    "bashfixture",
                    &assistant.id,
                    "Bash",
                    Some("{}"),
                    Some(BASH_OUTPUT),
                    false,
                    0,
                )?;
                Ok(())
            })
            .unwrap()
            .unwrap();
    }
    *state.lock().unwrap() = Provider {
        case: Some(case),
        ..Default::default()
    };
    let prompt = if matches!(case, Case::Snapshot(Entry::Manual)) {
        "/compact retain Bash output"
    } else {
        CURRENT
    };
    let result = resumed_query(&paths, temp.path(), &id, prompt).await;
    let diagnostic = format!(
        "{} {}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    let calls = state.lock().unwrap();
    let budget = deepseek_code_core::config::OrchestrationConfig::max_retries() as usize + 1;
    let native: Vec<_> = calls
        .posts
        .iter()
        .filter(|(kind, _)| *kind == "native")
        .collect();
    let summary: Vec<_> = calls
        .posts
        .iter()
        .filter(|(kind, _)| *kind == "summary")
        .collect();
    let order: Vec<_> = calls.posts.iter().map(|(kind, _)| *kind).collect();
    let boundaries = deepseek_code_core::services::compaction::count(&store.db, &id)
        .unwrap()
        .proven();

    eprintln!(
        "{case:?}: {} native, {} summary, {} main POSTs, {boundaries} boundaries",
        native.len(),
        summary.len(),
        calls.main
    );
    match case {
        Case::RejectedAfterRecovery => {
            let mut expected = vec![];
            for step in 0..10 {
                if matches!(step, 1 | 5) {
                    expected.extend(std::iter::repeat_n("native", budget));
                    expected.extend(std::iter::repeat_n("summary", budget));
                } else if step == 9 {
                    expected.push("native");
                }
                expected.push("main");
            }
            assert_eq!(order, expected, "{diagnostic}");
            assert_eq!(boundaries, 1);
            assert!(calls
                .posts
                .last()
                .unwrap()
                .1
                .to_string()
                .contains("recovered-at-nine"));
            assert!(!result.status.success());
            let terminal = diagnostic
                .rsplit_once("Error: ")
                .expect("CLI error footer")
                .1;
            assert!(terminal.contains("413"), "{terminal}");
            for stale in [
                "429",
                "503",
                "Automatic compaction failed",
                "uncompressed request",
            ] {
                assert!(!terminal.contains(stale), "{terminal}");
            }
        }
        Case::Overflow { cooldown, .. } => {
            assert_eq!(native.len(), budget, "{case:?} {order:?} {diagnostic}");
            assert!(!result.status.success(), "{diagnostic}");
            assert_eq!(summary.len(), budget, "{case:?} {order:?}");
            assert_eq!(calls.main, 1 + usize::from(cooldown));
            let mut expected = vec!["main"];
            expected.extend(std::iter::repeat_n("native", budget));
            expected.extend(std::iter::repeat_n("summary", budget));
            if cooldown {
                expected.push("main");
            }
            assert_eq!(order, expected, "{case:?}");
            assert_eq!(boundaries, 0);
            for cause in ["HTTP 429", "HTTP 503", "Resume and run /compact"] {
                assert!(diagnostic.contains(cause), "{case:?} {diagnostic}");
            }
        }
        Case::Snapshot(entry) => {
            assert_eq!(native.len(), budget, "{case:?} {order:?} {diagnostic}");
            assert!(result.status.success(), "{case:?} {diagnostic}");
            assert_eq!(boundaries, 1);
            assert_eq!(
                order[0] == "main",
                entry == Entry::MidTurn,
                "{case:?} {order:?}"
            );
            assert_eq!(
                calls.main,
                match entry {
                    Entry::Manual => 0,
                    Entry::MidTurn => 2,
                    _ => 1,
                }
            );
            assert_eq!(
                diagnostic.contains("Auto-compacting conversation"),
                matches!(entry, Entry::PreTurn | Entry::MidTurn),
                "{case:?} {diagnostic}"
            );
            for (_, input) in native {
                assert_eq!(
                    bash_output(input).as_deref(),
                    Some(BASH_OUTPUT),
                    "{case:?} native"
                );
            }
            let outputs: Vec<_> = summary
                .iter()
                .filter_map(|(_, input)| bash_output(input))
                .collect();
            assert_eq!(outputs, [BASH_OUTPUT], "{case:?} fallback");
            if entry == Entry::ProtectedPrefix {
                assert!(calls
                    .posts
                    .iter()
                    .all(|(kind, input)| input.to_string().contains(CURRENT) == (*kind == "main")));
            }
        }
    }
    server.abort();
}

#[tokio::test]
async fn provider_overflow_spends_one_budget_for_each_failure_streak() {
    for status in [
        StatusCode::PAYLOAD_TOO_LARGE,
        StatusCode::UNPROCESSABLE_ENTITY,
        StatusCode::OK,
    ] {
        for cooldown in [false, true] {
            run_case(Case::Overflow { status, cooldown }).await;
        }
    }
}

#[tokio::test]
async fn every_compaction_entry_preserves_structured_bash_in_both_transports() {
    for entry in [
        Entry::Manual,
        Entry::MidTurn,
        Entry::ProtectedPrefix,
        Entry::PreTurn,
    ] {
        run_case(Case::Snapshot(entry)).await;
    }
}

#[tokio::test]
async fn provider_rejection_after_a_successful_boundary_starts_a_new_streak() {
    run_case(Case::RejectedAfterRecovery).await;
}
