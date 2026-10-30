//! Standalone startup turns use local endpoints and isolated persistence.
use axum::{extract::State, routing::post, Json, Router};
use serde_json::{json, Value};
use std::{
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};

async fn hook(Json(body): Json<Value>) -> Json<Value> {
    Json(
        json!({"hookSpecificOutput":{"hookEventName":"SessionStart","initialUserMessage":format!("HOOK_{}",body["source"].as_str().unwrap())}}),
    )
}
async fn model(
    State(requests): State<Arc<Mutex<Vec<Value>>>>,
    Json(body): Json<Value>,
) -> impl axum::response::IntoResponse {
    requests.lock().unwrap().push(body);
    (
        [("content-type", "text/event-stream")],
        format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"index":0,"delta":{"content":"fixture completed"},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})
        ),
    )
}
#[tokio::test]
async fn print_initial_turn_once_before_caller_and_resume_only_uses_fresh_output() {
    for http in [false, true] {
        for caller in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let data = root.path().join("data");
            std::fs::create_dir(&data).unwrap();
            let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let app = Router::new()
                .route("/hook", post(hook))
                .route("/v1/chat/completions", post(model))
                .with_state(requests.clone());
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            std::fs::write(root.path().join("start.cjs"),r#"let s='';process.stdin.on('data',x=>s+=x);process.stdin.on('end',()=>console.log(JSON.stringify({hookSpecificOutput:{hookEventName:'SessionStart',initialUserMessage:'HOOK_'+JSON.parse(s).source}})));"#).unwrap();
            std::fs::write(root.path().join("submit.cjs"),r#"let s='';process.stdin.on('data',x=>s+=x);process.stdin.on('end',()=>require('fs').appendFileSync('submits.jsonl',s+'\n'));"#).unwrap();
            let command = if http {
                json!({"type":"http","url":format!("{endpoint}/hook")})
            } else {
                json!({"type":"command","command":"node start.cjs"})
            };
            let settings = json!({"hooks":{"SessionStart":[{"matcher":"startup","hooks":[command]}],"UserPromptSubmit":[{"hooks":[{"type":"command","command":"node submit.cjs"}]}]}});
            std::fs::write(data.join("settings.json"), settings.to_string()).unwrap();
            let id = uuid::Uuid::new_v4().to_string();
            for resume in [false, true] {
                let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_dcl"));
                command
                    .current_dir(root.path())
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
                        "INITIAL_HOOK_FIXTURE_KEY",
                        "--model",
                        "fixture",
                        "--print",
                        "--output-format",
                        "json",
                        if resume { "--resume" } else { "--session-id" },
                        &id,
                    ])
                    .env("INITIAL_HOOK_FIXTURE_KEY", "fixture-only");
                if caller || resume {
                    command.args(["--", "HUMAN_INPUT"]);
                }
                let output = tokio::time::timeout(Duration::from_secs(45), command.output())
                    .await
                    .unwrap()
                    .unwrap();
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                let result: Value = serde_json::from_slice(&output.stdout).unwrap();
                assert_eq!(result["result"], "fixture completed");
            }
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), if caller { 3 } else { 2 });
            assert!(requests[0].to_string().contains("HOOK_startup"));
            assert!(!requests[0].to_string().contains("HUMAN_INPUT"));
            let submits = std::fs::read_to_string(root.path().join("submits.jsonl")).unwrap();
            assert_eq!(
                submits
                    .lines()
                    .filter(|line| !line.trim().is_empty())
                    .count(),
                requests.len()
            );
            assert_eq!(submits.matches("HOOK_startup").count(), 1);
            let context = deepseek_code_core::services::session_messages::context(&data).unwrap();
            let authority =
                deepseek_code_core::services::permission_authority::history(&context.db, &id, 8192)
                    .unwrap();
            assert_eq!(authority.len(), if caller { 2 } else { 1 });
            assert!(!serde_json::to_string(&authority).unwrap().contains("HOOK_"));
            let count: i64=context.db.with_conn(|c| c.query_row("SELECT COUNT(*) FROM peripheral_settings WHERE key LIKE 'message-origin:%' AND value LIKE '%SessionStart%'",[],|r|r.get(0))).unwrap().unwrap();
            assert_eq!(count, 1);
            server.abort();
        }
    }
}
