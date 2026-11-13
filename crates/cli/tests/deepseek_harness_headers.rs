//! Real dcl processes, isolated SQLite, and a builtin DeepSeek local HTTP endpoint.
use axum::{extract::State, http::HeaderMap, response::IntoResponse, routing::post, Json, Router};
use deepseek_code_core::runtime::{RuntimePaths, SessionStore};
use serde_json::{json, Value};
use std::{
    process::Stdio,
    sync::{Arc, Mutex},
};
use tokio::process::Command;

#[derive(Clone)]
struct Fixture {
    api: &'static str,
    requests: Arc<Mutex<Vec<(HeaderMap, Value)>>>,
    recover: Arc<Mutex<bool>>,
}

async fn respond(
    State(state): State<Fixture>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> axum::response::Response {
    // Identify purpose from the body, independently of the headers under test.
    let compact = body
        .to_string()
        .contains("Write one continuation summary of the conversation above");
    state.requests.lock().unwrap().push((headers, body));
    if !compact && std::mem::take(&mut *state.recover.lock().unwrap()) {
        return (axum::http::StatusCode::BAD_REQUEST, Json(json!({"error":{
            "type":"invalid_request_error", "code":"context_length_exceeded", "message":"prompt is too long"
        }}))).into_response();
    }
    let text = if compact {
        "HEADER_COMPACT_SUMMARY"
    } else {
        "HEADER_NORMAL_REPLY"
    };
    let events = if state.api == "anthropic" {
        vec![
            json!({"type":"message_start","message":{"id":"fixture","type":"message","role":"assistant","model":"fixture-model","content":[],"usage":{"input_tokens":10,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":5}}),
            json!({"type":"message_stop"}),
        ]
    } else {
        vec![
            json!({"choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}),
        ]
    };
    let mut stream = events
        .into_iter()
        .map(|v| format!("data: {v}\n\n"))
        .collect::<String>();
    if state.api == "openai" {
        stream.push_str("data: [DONE]\n\n");
    }
    ([("content-type", "text/event-stream")], stream).into_response()
}

async fn run_cli(paths: &RuntimePaths, workspace: &std::path::Path, id: &str, prompt: &str) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dcl"));
    command
        .args(["--print", "--output-format", "stream-json", "--data-dir"])
        .arg(&paths.data_dir)
        .arg("--path")
        .arg(workspace)
        .args([
            "--provider",
            "fixture",
            "--model",
            "fixture-model",
            "--resume",
            id,
            prompt,
        ])
        .stdin(Stdio::null())
        .kill_on_drop(true);
    for (key, _) in std::env::vars() {
        if key.starts_with("DEEPSEEK_CODE") {
            command.env_remove(key);
        }
    }
    let output = tokio::time::timeout(std::time::Duration::from_secs(45), command.output())
        .await
        .unwrap()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let results: Vec<Value> = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|v| v["type"] == "result")
        .collect();
    assert_eq!(results.len(), 1, "{stdout}");
    assert_eq!(results[0]["turn"]["status"], "completed", "{stdout}");
    assert!(stdout.contains("HEADER_NORMAL_REPLY"));
}

async fn scenario(api: &'static str) {
    let root = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(root.path().join("data")).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let id = store
        .create(root.path(), "fixture-model", "fixture")
        .unwrap();
    let database_id: String = store
        .db
        .with_conn(|conn| {
            conn.query_row("SELECT id FROM conversations WHERE id=?1", [&id], |row| {
                row.get(0)
            })
        })
        .unwrap()
        .unwrap();
    let state = Fixture {
        api,
        requests: Default::default(),
        recover: Default::default(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .fallback(post(respond))
        .with_state(state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    // builtin_id identifies the platform while base_url keeps every request local.
    // Do not use --base-url: the CLI intentionally clears builtin_id for overrides.
    std::fs::write(paths.data_dir.join("providers.json"), json!([{
        "id":"fixture","name":"fixture","base_url":address,"api_key":"fixture-only",
        "api_format":api,"builtin_id":"deepseek","enabled":true,
        "models":[{"id":"fixture-model","name":"fixture","enabled":true,"context_window":128000,"max_tokens":4096,
            "supports_web_search":false,"supports_vision":false,"api":if api == "anthropic" {"anthropic-messages"} else {"openai-completions"}}]
    }]).to_string()).unwrap();
    run_cli(&paths, root.path(), &id, "ordinary header check").await;
    {
        let requests = state.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].0["x-deepseek-harness-session-id"], database_id);
        assert!(!requests[0].0.contains_key("x-deepseek-harness-compact"));
    }
    *state.recover.lock().unwrap() = true;
    run_cli(&paths, root.path(), &id, "continue with context recovery").await;
    server.abort();
    let requests = state.requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        4,
        "normal, overflow, summary, resumed normal"
    );
    for (index, (headers, body)) in requests.iter().enumerate() {
        assert_eq!(headers["x-deepseek-harness-session-id"], database_id);
        let compact = body
            .to_string()
            .contains("Write one continuation summary of the conversation above");
        assert_eq!(compact, index == 2);
        assert_eq!(
            headers
                .get("x-deepseek-harness-compact")
                .map(|h| h.to_str().unwrap()),
            compact.then_some("1")
        );
    }
    assert!(requests[3].1.to_string().contains("HEADER_COMPACT_SUMMARY"));
    assert!(store
        .history(&id)
        .unwrap()
        .iter()
        .any(|message| message["content"].as_str() == Some("HEADER_NORMAL_REPLY")));
    let boundaries: i64 = store
        .db
        .with_conn(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM messages WHERE conversation_id=?1 AND is_compact_boundary=1",
                [&id],
                |row| row.get(0),
            )
        })
        .unwrap()
        .unwrap();
    assert_eq!(boundaries, 1);
}

#[tokio::test]
async fn openai_cli_session_and_compaction_headers_match_database() {
    scenario("openai").await;
}
#[tokio::test]
async fn anthropic_cli_session_and_compaction_headers_match_database() {
    scenario("anthropic").await;
}
