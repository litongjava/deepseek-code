//! Actual CLI Write is blocked by a tool-free prompt evaluator.
use axum::{extract::State, routing::post, Json, Router};
use serde_json::{json, Value};
use std::{
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

async fn model(
    State(requests): State<Arc<Mutex<Vec<Value>>>>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    if body.get("tools").is_none() {
        assert_eq!(body["max_tokens"], 1024);
        let prompt = body["messages"][1]["content"].as_str().unwrap();
        let input: Value = serde_json::from_str(prompt.strip_prefix("Evaluate ").unwrap()).unwrap();
        requests.lock().unwrap().push(input);
        return Json(json!({"id":"hook","object":"chat.completion","created":0,"model":"fixture","choices":[{"index":0,"message":{"role":"assistant","content":"{\"ok\":false,\"reason\":\"Prompt fixture denied write\"}"},"finish_reason":"stop"}]})).into_response();
    }
    let done = body["messages"].as_array().unwrap().last().unwrap()["role"] == "tool";
    let (delta, finish) = if done {
        (json!({"content":"PROMPT fixture completed"}), "stop")
    } else {
        (
            json!({"tool_calls":[{"index":0,"id":"prompt-write","type":"function","function":{"name":"Write","arguments":json!({"file_path":"written.txt","content":"fixture output"}).to_string()}}]}),
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
        .into_response()
}

#[tokio::test]
async fn prompt_deny_prevents_real_write() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&data).unwrap();
    std::fs::create_dir(&workspace).unwrap();
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/chat/completions", post(model))
        .with_state(requests.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    std::fs::write(data.join("settings.json"), json!({"hooks":{"PreToolUse":[{"matcher":"Write","hooks":[{"type":"prompt","prompt":"Evaluate $ARGUMENTS","once":true,"timeout":5,"statusMessage":"PROMPT integration fixture"}]}]}}).to_string()).unwrap();
    let session = uuid::Uuid::new_v4().to_string();
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
                "PROMPT_HOOK_FIXTURE_KEY",
                "--model",
                "fixture",
                "--permission-mode",
                "bypass",
                "--print",
                "--session-id",
                &session,
                "--",
                "write the fixture file",
            ])
            .env("PROMPT_HOOK_FIXTURE_KEY", "fixture-only")
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
    assert!(String::from_utf8_lossy(&output.stdout).contains("PROMPT fixture completed"));
    assert!(!workspace.join("written.txt").exists());
    assert_eq!(requests.lock().unwrap().len(), 1);
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["session_id"], session);
    assert_eq!(requests[0]["hook_event_name"], "PreToolUse");
    assert_eq!(requests[0]["tool_name"], "Write");
    // Hooks receive the occurrence id; the raw id is provider provenance.
    assert!(requests[0]["tool_use_id"]
        .as_str()
        .is_some_and(|id| id.starts_with("inv-")));
    assert_ne!(requests[0]["tool_use_id"], "prompt-write");
    assert_eq!(requests[0]["tool_input"]["file_path"], "written.txt");
    server.abort();
}
