//! Actual CLI processes validate HTTP hook authority and durable once behavior.
use axum::{extract::State, routing::post, Json, Router};
use serde_json::{json, Value};
use std::{
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

async fn hook(
    State(requests): State<Arc<Mutex<Vec<Value>>>>,
    Json(body): Json<Value>,
) -> Json<Value> {
    requests.lock().unwrap().push(body);
    Json(
        json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"HTTP fixture denied first write"}}),
    )
}

async fn model(Json(body): Json<Value>) -> impl axum::response::IntoResponse {
    let done = body["messages"].as_array().unwrap().last().unwrap()["role"] == "tool";
    let (delta, finish) = if done {
        (json!({"content":"HTTP fixture completed"}), "stop")
    } else {
        (
            json!({"tool_calls":[{"index":0,"id":"http-write","type":"function","function":{"name":"Write","arguments":json!({"file_path":"written.txt","content":"fixture output"}).to_string()}}]}),
            "tool_calls",
        )
    };
    (
        [("content-type", "text/event-stream")],
        format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":finish}]})
        ),
    )
}

#[tokio::test]
async fn http_deny_prevents_real_write_and_once_is_not_replayed_after_resume() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&data).unwrap();
    std::fs::create_dir(&workspace).unwrap();
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let hook_endpoint = endpoint.replace("127.0.0.1", "localhost");
    let app = Router::new()
        .route("/hook", post(hook))
        .route("/v1/chat/completions", post(model))
        .with_state(requests.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    std::fs::write(data.join("settings.json"), json!({"hooks":{"PreToolUse":[{"matcher":"Write","hooks":[{"type":"http","url":format!("{hook_endpoint}/hook"),"once":true,"timeout":5,"statusMessage":"HTTP integration fixture"}]}]}}).to_string()).unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    for resume in [false, true] {
        let output = tokio::time::timeout(
            Duration::from_secs(30),
            tokio::process::Command::new(env!("CARGO_BIN_EXE_dcl"))
                .current_dir(&workspace)
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .args([
                    "--data-dir",
                    data.to_str().unwrap(),
                    "--base-url",
                    &endpoint,
                    "--api-format",
                    "openai",
                    "--api-key-env",
                    "HTTP_HOOK_FIXTURE_KEY",
                    "--model",
                    "fixture",
                    "--permission-mode",
                    "bypass",
                    "--print",
                    if resume { "--resume" } else { "--session-id" },
                    &session,
                    "--",
                    "write the fixture file",
                ])
                .env("HTTP_HOOK_FIXTURE_KEY", "fixture-only")
                // A dead proxy must not intercept the local HTTP hook request.
                .env("HTTP_PROXY", "http://127.0.0.1:1")
                .env("HTTPS_PROXY", "http://127.0.0.1:1")
                .env("ALL_PROXY", "http://127.0.0.1:1")
                // Only the model endpoint is exempted; localhost is not.
                .env("NO_PROXY", "127.0.0.1")
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("HTTP fixture completed"));
        assert_eq!(workspace.join("written.txt").exists(), resume);
        assert_eq!(requests.lock().unwrap().len(), 1);
        if resume {
            assert_eq!(
                std::fs::read_to_string(workspace.join("written.txt")).unwrap(),
                "fixture output"
            );
        }
    }
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["session_id"], session);
    assert_eq!(requests[0]["hook_event_name"], "PreToolUse");
    assert_eq!(requests[0]["tool_name"], "Write");
    // Hooks receive the occurrence id; the raw id is provider provenance.
    assert!(requests[0]["tool_use_id"]
        .as_str()
        .is_some_and(|id| id.starts_with("inv-")));
    assert_ne!(requests[0]["tool_use_id"], "http-write");
    assert_eq!(requests[0]["tool_input"]["file_path"], "written.txt");
    server.abort();
}
