//! Actual CLI + local provider: persisted accounting corresponds to transmitted input.
use axum::{extract::State, routing::post, Json, Router};
use deepseek_code_core::{
    runtime::{RuntimePaths, SessionStore},
    services::context_usage,
};
use serde_json::{json, Value};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

async fn reply(
    State(requests): State<Arc<Mutex<Vec<Value>>>>,
    Json(body): Json<Value>,
) -> impl axum::response::IntoResponse {
    requests.lock().unwrap().push(body);
    (
        [("content-type", "text/event-stream")],
        format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"index":0,"delta":{"content":"Fixture response"},"finish_reason":"stop"}],"usage":{"prompt_tokens":123,"completion_tokens":2,"total_tokens":125,"prompt_tokens_details":{"cached_tokens":20}}})
        ),
    )
}

#[tokio::test]
async fn cli_persists_final_request_accounting_and_inspects_it_without_network() {
    let temp = tempfile::tempdir().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new()
        .route("/v1/chat/completions", post(reply))
        .with_state(requests.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let session = uuid::Uuid::new_v4().to_string();
    let data = temp.path().join("data");
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_dcl"));
    command
        .current_dir(temp.path())
        .env("CLAUDE_CONFIG_DIR", temp.path().join("config"))
        .env("CONTEXT_FIXTURE_KEY", "PRIVATE_DUMMY_KEY")
        .env("NO_COLOR", "1")
        .args([
            "--data-dir",
            data.to_str().unwrap(),
            "--base-url",
            &endpoint,
            "--api-format",
            "openai",
            "--api-key-env",
            "CONTEXT_FIXTURE_KEY",
            "--model",
            "fixture",
            "--session-id",
            &session,
            "--print",
            "--",
            "PRIVATE_CONTEXT_QUESTION",
        ])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let output = tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    server.abort();
    let paths = RuntimePaths::new(data).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let snapshot = context_usage::last_request(&store.db, &session)
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.reported_input_tokens, Some(123));
    let usage = snapshot.usage.unwrap();
    let sent = requests.lock().unwrap();
    assert_eq!(sent.len(), 1);
    let body = &sent[0];
    assert_eq!(usage.sent_output_reserve, body["max_tokens"].as_u64());
    assert_eq!(
        usage.tools.serialized_utf8_bytes,
        serde_json::to_vec(&body["tools"]).unwrap().len() as u64
    );
    let messages = body["messages"].as_array().unwrap();
    let systems: u64 = messages
        .iter()
        .filter(|m| m["role"] == "system" || m["role"] == "developer")
        .map(|m| serde_json::to_vec(m).unwrap().len() as u64)
        .sum();
    assert_eq!(usage.instructions.serialized_utf8_bytes, systems);
    assert!(usage.history.estimated_tokens > 0);
    let encoded = serde_json::to_string(&usage).unwrap();
    assert!(!encoded.contains("PRIVATE"));
    assert!(!encoded.contains("http"));
    store
        .append(&session, "user", "next input not sent yet")
        .unwrap();
    assert_eq!(
        context_usage::last_request(&store.db, &session)
            .unwrap()
            .unwrap()
            .reported_input_tokens,
        Some(123)
    );
}
