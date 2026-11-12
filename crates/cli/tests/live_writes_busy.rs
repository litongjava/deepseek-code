//! A live checkpoint copy is best effort: while another process holds the
//! session database writer, a print-mode turn must still finish and print the
//! model reply instead of failing the turn on SQLITE_BUSY.
use axum::{extract::State, response::IntoResponse, routing::post, Json, Router};
use serde_json::{json, Value};
use std::{
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{process::Command, sync::oneshot};

#[derive(Clone, Default)]
struct Fixture {
    started: Arc<Mutex<Option<oneshot::Sender<()>>>>,
}

async fn respond(
    State(fixture): State<Fixture>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    assert_eq!(body["stream"], true);
    if let Some(started) = fixture.started.lock().unwrap().take() {
        let _ = started.send(());
    }
    // Give the blocker time to take the write lock before streaming starts.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let frames = [
        json!({"choices":[{"index":0,"delta":{"content":"busy-safe reply"},"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
    ];
    let mut stream: String = frames
        .into_iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect();
    stream.push_str("data: [DONE]\n\n");
    ([("content-type", "text/event-stream")], stream).into_response()
}

#[tokio::test]
async fn print_turn_survives_a_busy_database_during_live_writes() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let (started, arrived) = oneshot::channel();
    let fixture = Fixture { started: Arc::new(Mutex::new(Some(started))) };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/chat/completions", post(respond))
        .with_state(fixture.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    std::fs::write(
        data.join("providers.json"),
        json!([{
            "id":"fixture","name":"Fixture","base_url":base,"api_key":"fixture-only",
            "api_format":"openai","enabled":true,
            "models":[{"id":"fixture-model","name":"Fixture","enabled":true,
                "context_window":128000,"max_tokens":4096,
                "supports_web_search":false,"supports_vision":false}]
        }])
        .to_string(),
    )
    .unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let mut cmd = Command::new(
        std::env::var_os("CLAUDE_SDK_TEST_EXE").unwrap_or_else(|| env!("CARGO_BIN_EXE_dcl").into()),
    );
    cmd.args(["--print", "--data-dir"])
        .arg(&data)
        .arg("--path")
        .arg(root.path())
        .args([
            "--provider",
            "fixture",
            "--model",
            "fixture-model",
            "--permission-mode",
            "bypassPermissions",
            "--session-id",
            &id,
            "fixture busy turn",
        ])
        .stdin(Stdio::null())
        .kill_on_drop(true);
    for (key, _) in std::env::vars() {
        if key.starts_with("DEEPSEEK_CODE") {
            cmd.env_remove(key);
        }
    }
    let child = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(90), cmd.output())
            .await
            .unwrap()
            .unwrap()
    });
    // The model request is in flight: hold the session database writer, like
    // another process did during the observed live checkpoint.
    arrived.await.unwrap();
    let db_path = data.join("deepseek_code.db");
    let blocker = std::thread::spawn(move || {
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        conn.busy_timeout(Duration::from_secs(10)).unwrap();
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        std::thread::sleep(Duration::from_secs(7));
        conn.execute_batch("COMMIT").unwrap();
    });
    let output = child.await.unwrap();
    blocker.join().unwrap();
    server.abort();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("busy-safe reply"),
        "missing model reply: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}
