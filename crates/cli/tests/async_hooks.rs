//! Standalone CLI/model fixture: async hook does not hold up the first model
//! request, and its informational output reaches the next request exactly once.
use axum::{extract::State, routing::post, Json, Router};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Clone)]
struct Fixture {
    root: PathBuf,
    requests: Arc<Mutex<Vec<Value>>>,
}

async fn model(
    State(state): State<Fixture>,
    Json(body): Json<Value>,
) -> impl axum::response::IntoResponse {
    let index = {
        let mut requests = state.requests.lock().unwrap();
        let index = requests.len();
        requests.push(body);
        index
    };
    if index == 0 {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !state.root.join("started").exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        // Only the first model request can release this hook: synchronous
        // execution would deadlock until its timeout and fail these assertions.
        assert!(!state.root.join("finished").exists());
        std::fs::write(state.root.join("release"), "").unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while !state.root.join("finished").exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    let (delta, finish) = if index == 0 {
        (
            json!({"tool_calls":[{"index":0,"id":"async-write","type":"function","function":{"name":"Write","arguments":json!({"file_path":"written.txt","content":"allowed"}).to_string()}}]}),
            "tool_calls",
        )
    } else {
        (json!({"content":"Async fixture completed"}), "stop")
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
async fn command_async_delivers_on_next_model_request_and_once_survives_cli_resume() {
    busy_fixture(false).await;
}

#[tokio::test]
async fn command_rewake_delivers_while_busy_without_permission_or_resume_replay() {
    busy_fixture(true).await;
}

async fn busy_fixture(rewake: bool) {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    let workspace = root.path().join("workspace");
    std::fs::create_dir(&data).unwrap();
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("hook.cjs"),r#"
const fs=require('fs'); process.stdin.resume(); process.stdin.on('end',()=>{
fs.appendFileSync('runs','x'); fs.writeFileSync('started','');
const timer=setInterval(()=>{if(!fs.existsSync('release'))return;clearInterval(timer);
console.log(JSON.stringify({systemMessage:'ASYNC_SYSTEM_SENTINEL',continue:false,decision:'block',hookSpecificOutput:{hookEventName:'SessionStart',additionalContext:'ASYNC_CONTEXT_SENTINEL',permissionDecision:'deny'}}));
fs.writeFileSync('finished','');process.exit(2);},20);});
"#).unwrap();
    deepseek_code_core::data_layout::write_config_file(data.join("config/settings.json"),json!({"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"node hook.cjs","async":true,"asyncRewake":rewake,"once":true,"timeout":20}]}]}}).to_string()).unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/chat/completions", post(model))
        .with_state(Fixture {
            root: workspace.clone(),
            requests: requests.clone(),
        });
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let session = uuid::Uuid::new_v4().to_string();
    for resume in [false, true] {
        let output = tokio::time::timeout(
            Duration::from_secs(35),
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
                    "ASYNC_FIXTURE_KEY",
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
                .env("ASYNC_FIXTURE_KEY", "fixture-only")
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
        assert!(String::from_utf8_lossy(&output.stdout).contains("Async fixture completed"));
        assert_eq!(
            std::fs::read_to_string(workspace.join("runs")).unwrap(),
            "x"
        );
    }
    assert_eq!(
        std::fs::read_to_string(workspace.join("written.txt")).unwrap(),
        "allowed"
    );
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(!requests[0].to_string().contains("ASYNC_SYSTEM_SENTINEL"));
    for marker in ["ASYNC_SYSTEM_SENTINEL", "ASYNC_CONTEXT_SENTINEL"] {
        assert_eq!(requests[1].to_string().matches(marker).count(), 1);
        assert!(
            !requests[2].to_string().contains(marker),
            "async output replayed on resume"
        );
    }
    server.abort();
}
