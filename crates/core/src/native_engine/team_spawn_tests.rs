use super::*;
use crate::{
    native_engine::{engine_core::ReasoningMode, provider_manager::ResolvedProvider},
    services::teams,
};
use axum::{extract::State, routing::post, Router};
use std::{
    path::Path,
    sync::{Arc, Mutex, Once},
    time::Duration,
};

fn enable_teams() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| std::env::set_var("DEEPSEEK_CODE_EXPERIMENTAL_AGENT_TEAMS", "1"));
}

#[derive(Default)]
struct Overlap {
    arrivals: usize,
    arrivals_at_first_response: Option<usize>,
}

async fn executor(
    root: &Path,
    parent: &str,
    endpoint: String,
) -> (ToolLoopExecutor, crate::services::ServiceContext) {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let context = ServiceContext::open(
        RuntimePaths::new(root.join("data")).unwrap(),
        Arc::new(EnvironmentCredentials),
    )
    .unwrap();
    context
        .db
        .create(
            parent,
            crate::db::session_store::CreateOptions {
                model: Some("fixture".into()),
                provider: Some("fixture".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let private_root = context.paths.data_dir.join("data/workspaces");
    let cwd = context
        .db
        .engine_workspace(&private_root, parent, None)
        .unwrap()
        .0;
    std::fs::create_dir_all(&cwd).unwrap();
    let provider = ResolvedProvider {
        provider: serde_json::from_value(json!({"id":"fixture","name":"fixture","base_url":endpoint,"api_key":"unused","api_format":"openai","models":[],"enabled":true})).unwrap(),
        model: serde_json::from_value(json!({"id":"fixture","name":"fixture","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap(),
    };
    let (events, _receiver) = tokio::sync::mpsc::channel(128);
    let executor = ToolLoopExecutor::new(
        provider,
        vec![],
        None,
        128,
        events,
        cwd.to_string_lossy().into_owned(),
        ReasoningMode::Standard,
    )
    .with_conv_id(parent.to_string())
    .with_search_config_path(context.paths.layout().config_file("settings.json"));
    (executor, context)
}

fn completion() -> String {
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0,"delta":{"content":"done"},"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_runs_named_teammates_concurrently_with_distinct_sessions() {
    enable_teams();
    let overlap = Arc::new(Mutex::new(Overlap::default()));
    let app = Router::new()
        .route(
            "/v1/chat/completions",
            post(|State(overlap): State<Arc<Mutex<Overlap>>>| async move {
                let arrival = {
                    let mut state = overlap.lock().unwrap();
                    state.arrivals += 1;
                    state.arrivals
                };
                let _ = arrival;
                // Both children must be in flight before either response exists;
                // a sequential implementation times out here and fails the check.
                let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
                while overlap.lock().unwrap().arrivals < 2 {
                    if tokio::time::Instant::now() >= deadline {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                let mut state = overlap.lock().unwrap();
                let arrivals = state.arrivals;
                state.arrivals_at_first_response.get_or_insert(arrivals);
                drop(state);
                ([("content-type", "text/event-stream")], completion())
            }),
        )
        .with_state(overlap.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let temp = tempfile::tempdir().unwrap();
    let parent = uuid::Uuid::new_v4().to_string();
    let (executor, _context) = executor(temp.path(), &parent, endpoint).await;
    let root = temp.path().join("data");
    teams::create(
        &root,
        &parent,
        "fixture",
        Some("concurrency fixture"),
        None,
        None,
        Path::new(&executor.workspace_cwd),
    )
    .unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(20),
        executor.execute_team_spawn(&json!({
            "team_name":"fixture",
            "members":[
                {"name":"alpha","prompt":"Say done."},
                {"name":"beta","prompt":"Say done."}
            ]
        })),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result["team_name"], "fixture");
    let members = result["members"].as_array().unwrap();
    assert_eq!(members.len(), 2);
    assert!(members.iter().all(|member| member["status"] == "completed"));
    let sessions: std::collections::HashSet<_> = members
        .iter()
        .map(|member| member["session_id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(sessions.len(), 2);
    assert_ne!(sessions.iter().next().unwrap(), &parent);
    assert_eq!(
        overlap.lock().unwrap().arrivals_at_first_response,
        Some(2),
        "both teammates must be in flight before the first response"
    );

    let team = teams::read_team(&root, "fixture").unwrap().unwrap();
    for name in ["alpha", "beta"] {
        let member = team
            .members
            .iter()
            .find(|member| member.name == name)
            .unwrap();
        assert_eq!(member.agent_id, format!("{name}@fixture"));
        assert!(member.session_id.is_some());
        assert!(!member.is_active);
        assert_eq!(member.status.as_deref(), Some("idle"));
        assert_eq!(member.agent_type.as_deref(), Some("general"));
    }
    assert!(teams::delete(&root, &parent).unwrap().success);
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelled_spawn_marks_members_stopped_and_allows_delete() {
    enable_teams();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|| async {
            tokio::time::sleep(Duration::from_secs(30)).await;
            ([("content-type", "text/event-stream")], completion())
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let temp = tempfile::tempdir().unwrap();
    let parent = uuid::Uuid::new_v4().to_string();
    let (executor, _context) = executor(temp.path(), &parent, endpoint).await;
    let root = temp.path().join("data");
    teams::create(
        &root,
        &parent,
        "stuck",
        None,
        None,
        None,
        Path::new(&executor.workspace_cwd),
    )
    .unwrap();
    let spawned = tokio::time::timeout(
        Duration::from_millis(700),
        executor.execute_team_spawn(&json!({
            "team_name":"stuck",
            "members":[
                {"name":"alpha","prompt":"Wait forever."},
                {"name":"beta","prompt":"Wait forever."}
            ]
        })),
    )
    .await;
    assert!(spawned.is_err(), "spawn should still be waiting");

    // Dropping the leader turn must not leave phantom active members or
    // background executors that block cleanup.
    let team = teams::read_team(&root, "stuck").unwrap().unwrap();
    for name in ["alpha", "beta"] {
        let member = team
            .members
            .iter()
            .find(|member| member.name == name)
            .unwrap();
        assert!(!member.is_active);
        assert_eq!(member.status.as_deref(), Some("cancelled"));
    }
    let removed = teams::delete(&root, &parent).unwrap();
    assert!(removed.success, "{}", removed.message);
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn teammate_idle_hook_receives_identity_and_blocking_feedback_resumes_the_teammate() {
    use crate::services::tool_hooks::HookRunner;
    enable_teams();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|| async { ([("content-type", "text/event-stream")], completion()) }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let temp = tempfile::tempdir().unwrap();
    let payloads = temp.path().join("idle-payloads.txt");
    let command = if cfg!(windows) {
        format!(
            "$j=[Console]::In.ReadToEnd(); Add-Content -LiteralPath '{}' -Value $j; exit 2",
            payloads.display()
        )
    } else {
        format!(
            "cat >> '{}'; printf '\n' >> '{}'; exit 2",
            payloads.display(),
            payloads.display()
        )
    };
    std::fs::create_dir_all(temp.path().join("data")).unwrap();
    crate::data_layout::write_config_file(
        temp.path().join("data/config/settings.json"),
        json!({"hooks":{"TeammateIdle":[{"hooks":[{"type":"command","command":command,"shell":if cfg!(windows){"powershell"}else{"sh"}}]}]}})
            .to_string(),
    )
    .unwrap_or_else(|error| panic!("settings: {error}"));
    let parent = uuid::Uuid::new_v4().to_string();
    let (executor, _context) = executor(temp.path(), &parent, endpoint).await;
    let executor =
        executor.with_hooks(HookRunner::load(&temp.path().join("data"), temp.path()).unwrap());
    let root = temp.path().join("data");
    teams::create(
        &root,
        &parent,
        "fixture",
        None,
        None,
        None,
        Path::new(&executor.workspace_cwd),
    )
    .unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(20),
        executor.execute_team_spawn(&json!({
            "team_name":"fixture",
            "members":[{"name":"alpha","prompt":"alpha work"}]
        })),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result["members"][0]["status"], "completed");

    let recorded = std::fs::read_to_string(&payloads).unwrap();
    let events = recorded
        .matches("\"hook_event_name\":\"TeammateIdle\"")
        .count();
    assert!(events >= 2, "{recorded}");
    assert_eq!(
        recorded.matches("\"teammate_name\":\"alpha\"").count(),
        events
    );
    assert_eq!(
        recorded.matches("\"team_name\":\"fixture\"").count(),
        events
    );
    let team = teams::read_team(&root, "fixture").unwrap().unwrap();
    let member = team.members.iter().find(|m| m.name == "alpha").unwrap();
    assert!(!member.is_active);
    assert_eq!(member.status.as_deref(), Some("idle"));
    assert!(teams::delete(&root, &parent).unwrap().success);
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_search_scopes_parent_agent_teammate_and_owned_resume() {
    use crate::services::compaction;
    use axum::Json;

    enable_teams();
    let temp = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let parent = uuid::Uuid::new_v4().to_string();
    let (executor, context) = executor(temp.path(), &parent, endpoint).await;
    let mut executor = executor.with_goal_state(context.db.clone());
    let store = SessionStore::new(context.db.clone());
    store
        .append(&parent, "user", "scope-secret-parent")
        .unwrap();
    store.append(&parent, "assistant", "parent reply").unwrap();
    let compact = |session: &str| {
        let prepared = compaction::prepare(&store.db, session).unwrap();
        compaction::commit(&store.db, prepared, "Summary omits the marker")
            .unwrap()
            .message_id
    };
    let parent_boundary = compact(&parent);
    let foreign = parent_boundary.clone();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let captured = seen.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |Json(body): Json<Value>| {
            let captured = captured.clone();
            let foreign = foreign.clone();
            async move {
                let messages = body["messages"].as_array().unwrap();
                let (index, label) = messages.iter().enumerate().rev()
                    .filter(|(_, m)| m["role"] == "user")
                    .find_map(|(index, m)| {
                        let text = m["content"].as_str()?;
                        ["Agent probe", "Team probe", "Resume team"].into_iter()
                            .find(|label| text.contains(label))
                            .map(|label| (index, label))
                    }).expect("child probe prompt");
                let outputs: Vec<_> = messages[index + 1..].iter()
                    .filter(|m| m["role"] == "tool")
                    .filter_map(|m| m["content"].as_str())
                    .collect();
                let loaded = body["tools"].as_array().unwrap().iter()
                    .any(|tool| tool["function"]["name"] == "ContextSearch");
                let searches: Vec<_> = outputs.iter().copied()
                    // ToolLoad now reaches the model as its content line rather
                    // than a JSON marker, so exclude that line by shape and
                    // keep every other result, including search errors.
                    .filter(|output| {
                        !(output.starts_with("Loaded ")
                            || output.starts_with("Already available: ")
                            || output.starts_with("Unknown tools: ")
                            || output.starts_with("Unavailable tools: "))
                    })
                    .collect();
                let call = match searches.len() {
                    0 if !loaded && outputs.is_empty() => Some(("load", "ToolLoad", json!({"names":["ContextSearch"]}))),
                    0 => Some(("own", "ContextSearch", json!({"query":"scope-secret-","role":"user"}))),
                    1 => Some(("foreign", "ContextSearch", json!({"query":"scope-secret-","range":"before","boundary_id":foreign}))),
                    _ => {
                        captured.lock().unwrap().push((
                            label,
                            searches.into_iter().map(str::to_owned).collect::<Vec<_>>(),
                        ));
                        None
                    }
                };
                let response = if let Some((id, name, input)) = call {
                    let id = format!("{}-{id}", label.replace(' ', "-"));
                    let delta = json!({"choices":[{"index":0,"delta":{"tool_calls":[{
                        "index":0,"id":id,"type":"function","function":{"name":name,"arguments":input.to_string()}
                    }]},"finish_reason":null}]});
                    let end = json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]});
                    format!("data: {delta}\n\ndata: {end}\n\ndata: [DONE]\n\n")
                } else {
                    completion()
                };
                ([("content-type", "text/event-stream")], response)
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let agent = executor
        .execute_delegated(&json!({
            "prompt":"Agent probe scope-secret-agent","run_in_background":false
        }))
        .await
        .unwrap();
    let agent_id = agent["session_id"].as_str().unwrap();
    let root = context.paths.data_dir.as_path();
    teams::create(
        root,
        &parent,
        "fixture",
        None,
        None,
        None,
        Path::new(&executor.workspace_cwd),
    )
    .unwrap();
    let team = executor.execute_team_spawn(&json!({
        "team_name":"fixture","members":[{"name":"alpha","prompt":"Team probe scope-secret-team"}]
    })).await.unwrap();
    assert_eq!(team["members"][0]["status"], "completed", "{team}");
    let team_id = team["members"][0]["session_id"].as_str().unwrap();
    let boundaries = [compact(agent_id), compact(team_id)];
    executor
        .resume_owned_child(
            team_id,
            Some("Resume team without the original words"),
            false,
        )
        .await
        .unwrap();

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 3);
    for (label, outputs) in seen.iter() {
        let own: Value = serde_json::from_str(&outputs[0]).unwrap();
        let hits = own["matches"].as_array().unwrap();
        assert_eq!(hits.len(), 1, "{label}: {own}");
        let marker = if *label == "Agent probe" {
            "scope-secret-agent"
        } else {
            "scope-secret-team"
        };
        assert!(
            hits[0]["excerpt"].as_str().unwrap().contains(marker),
            "{label}: {own}"
        );
        assert!(
            outputs[1].contains("Unknown compaction boundary in this session"),
            "{outputs:?}"
        );
        if *label == "Resume team" {
            assert_eq!(own["boundary_id"], boundaries[1]);
        }
    }
    drop(seen);

    let (_, output, failed) = executor
        .execute_tool_call(
            "ToolLoad",
            &json!({"names":["ContextSearch"]}),
            "load-parent",
        )
        .await;
    assert!(!failed && output.contains("ContextSearch"));
    let (_, output, failed) = executor
        .execute_tool_call(
            "ContextSearch",
            &json!({"query":"scope-secret-","role":"user"}),
            "parent-own",
        )
        .await;
    assert!(!failed, "{output}");
    let result: Value = serde_json::from_str(&output).unwrap();
    let hits = result["matches"].as_array().unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["excerpt"], "scope-secret-parent");
    for boundary in boundaries {
        let (_, output, failed) = executor
            .execute_tool_call(
                "ContextSearch",
                &json!({"query":"scope-secret-","range":"before","boundary_id":boundary}),
                "parent-foreign",
            )
            .await;
        assert!(
            failed && output.contains("Unknown compaction boundary in this session"),
            "{output}"
        );
    }

    let mut restricted = executor
        .prepare_delegated_child(
            &json!({"prompt":"Restricted child","agent_type":"general"}),
            None,
            Some(vec!["Read".into()]),
            None,
        )
        .await
        .unwrap();
    let (_, output, failed) = restricted
        .child
        .execute_tool_call("ContextSearch", &json!({"query":"scope-secret-"}), "denied")
        .await;
    assert!(
        failed && output.contains("not allowed for this agent"),
        "{output}"
    );
    server.abort();
}

#[tokio::test]
async fn foreground_agent_uses_child_memory_after_parent_index_changes() {
    use crate::services::memory_files::FileMemory;
    let temp = tempfile::tempdir().unwrap();
    let parent = uuid::Uuid::new_v4().to_string();
    let (executor, context) = executor(temp.path(), &parent, "http://127.0.0.1:1".into()).await;
    let memory = FileMemory::session(&context, &parent).unwrap();
    memory.store(false).unwrap();
    let index = memory.directory.join("MEMORY.md");
    let executor = executor.with_context_reminders(vec!["PARENT_ONLY_INDEX".into()]);
    std::fs::write(&index, "CHILD_FROZEN_INDEX").unwrap();
    let prepared = executor
        .prepare_delegated_child(
            &json!({"prompt":"inspect","agent_type":"general"}),
            None,
            None,
            None,
        )
        .await
        .unwrap();
    std::fs::write(index, "LATER_INDEX").unwrap();
    let shard = context.db.open_session(&prepared.id).unwrap();
    let (messages, boundary) = shard
        .read(|r| r.model_history(&crate::native_engine::provider_manager::ApiFormat::OpenAI))
        .unwrap();
    let (stored, _) =
        crate::services::memory_files::prompt::replay(&shard, boundary.as_deref(), &messages)
            .unwrap();
    let stored = stored.join("\n");
    assert!(stored.contains("CHILD_FROZEN_INDEX"));
    let request = serde_json::to_value(prepared.child.build_openai_messages().unwrap()).unwrap();
    let first = request[0]["content"].as_str().unwrap();
    assert!(first.contains(&stored));
    assert!(!first.contains("PARENT_ONLY_INDEX"));
}
