use super::*;
use crate::{
    db::conversation_repo,
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
    ONCE.call_once(|| std::env::set_var("CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS", "1"));
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
        .with_conn(|conn| -> Result<()> {
            conversation_repo::insert_conversation(
                conn,
                parent,
                None,
                Some("fixture"),
                Some("fixture"),
                root.to_str(),
                None,
                false,
                false,
                false,
                "now",
                "now",
                0,
            )?;
            Ok(())
        })
        .unwrap()
        .unwrap();
    let private_root = root.join("private-root");
    let cwd = context
        .db
        .with_conn(|conn| {
            crate::native_engine::workspace::resolve_workspace(conn, &private_root, parent, None)
        })
        .unwrap()
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
    .with_search_config_path(context.paths.data_dir.join("settings.json"));
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
    std::fs::write(
        temp.path().join("data").join("settings.json"),
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
