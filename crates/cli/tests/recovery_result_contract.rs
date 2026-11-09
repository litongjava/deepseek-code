use axum::{extract::State, response::IntoResponse, routing::post, Json, Router};
use deepseek_code_core::runtime::{RuntimePaths, SessionStore};
use serde_json::{json, Value};
use std::{
    process::Stdio,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use tokio::process::Command;

#[derive(Clone)]
struct Fixture {
    main: Arc<AtomicUsize>,
    summaries: Arc<AtomicUsize>,
    summary_fails: bool,
    retry_fails: bool,
}

async fn respond(
    State(state): State<Fixture>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    if body["stream"] != true {
        state.summaries.fetch_add(1, Ordering::SeqCst);
        let summary = if state.summary_fails {
            "s".repeat(32_001)
        } else {
            "valid compact summary".into()
        };
        return Json(
            json!({"id":"summary","object":"chat.completion","created":0,"model":"fixture-model",
            "choices":[{"index":0,"message":{"role":"assistant","content":summary},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}),
        )
        .into_response();
    }
    let index = state.main.fetch_add(1, Ordering::SeqCst);
    if index == 0 || state.retry_fails {
        return (axum::http::StatusCode::BAD_REQUEST, Json(json!({"error":{
            "type":"invalid_request_error","code":"context_length_exceeded","message":"prompt is too long"}
        }))).into_response();
    }
    let stream = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"recovered\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":23,\"completion_tokens\":5}}\n\n",
        "data: [DONE]\n\n"
    );
    ([("content-type", "text/event-stream")], stream).into_response()
}

#[tokio::test]
async fn recovery_results_keep_the_sdk_contract_and_logging_is_nonfatal() {
    for mode in [
        "no-history",
        "success",
        "summary-failed",
        "retry-failed",
        "log-failure",
        "log-summary-failed",
    ] {
        let root = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(root.path().join("data")).unwrap();
        let store = SessionStore::new(paths.open_database().unwrap());
        let id = store
            .create(root.path(), "fixture-model", "fixture")
            .unwrap();
        if mode != "no-history" {
            store.append(&id, "user", "seed task").unwrap();
            store.append(&id, "assistant", "seed answer").unwrap();
        }
        let state = Fixture {
            main: Arc::default(),
            summaries: Arc::default(),
            summary_fails: mode.contains("summary-failed"),
            retry_fails: mode == "retry-failed",
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/v1/chat/completions", post(respond))
            .with_state(state.clone());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        std::fs::write(paths.data_dir.join("providers.json"),json!([{
            "id":"fixture","name":"fixture","base_url":address,"api_key":"fixture-only","api_format":"openai","enabled":true,
            "models":[{"id":"fixture-model","name":"Fixture","enabled":true,"context_window":128000,"max_tokens":4096,"supports_web_search":false,"supports_vision":false}]
        }]).to_string()).unwrap();
        if mode.starts_with("log-") {
            std::fs::create_dir_all(
                paths
                    .data_dir
                    .join("logs/request-attempts")
                    .join(format!("{id}.jsonl")),
            )
            .unwrap();
        }
        let mut command = Command::new(
            std::env::var_os("CLAUDE_SDK_TEST_EXE")
                .unwrap_or_else(|| env!("CARGO_BIN_EXE_dcl").into()),
        );
        command
            .args(["--print", "--output-format", "stream-json", "--data-dir"])
            .arg(&paths.data_dir)
            .args(["--path"])
            .arg(root.path())
            .args([
                "--provider",
                "fixture",
                "--model",
                "fixture-model",
                "--resume",
                &id,
                "continue",
            ])
            .stdin(Stdio::null())
            .kill_on_drop(true);
        for (key, _) in std::env::vars() {
            if key.starts_with("DEEPSEEK_CODE") || key.starts_with("CLAUDE_RUST") {
                command.env_remove(key);
            }
        }
        let output = tokio::time::timeout(std::time::Duration::from_secs(40), command.output())
            .await
            .unwrap()
            .unwrap();
        server.abort();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let events: Vec<Value> = stdout
            .lines()
            .filter(|l| l.starts_with('{'))
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let results: Vec<_> = events.iter().filter(|e| e["type"] == "result").collect();
        assert_eq!(results.len(), 1, "{mode}: {stdout}");
        let result = results[0];
        for key in [
            "usage",
            "num_turns",
            "total_cost_usd",
            "permission_denials",
            "structured_output",
            "stop_reason",
        ] {
            assert!(result.get(key).is_some(), "{mode}: missing {key}: {result}");
        }
        assert!(result["usage"].is_object() && result["permission_denials"].is_array());
        let succeeds = matches!(mode, "success" | "log-failure");
        assert_eq!(output.status.success(), succeeds, "{mode}: {result}");
        assert_eq!(
            result["turn"]["status"],
            if succeeds { "completed" } else { "failed" }
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| e["type"] == "turn_timing" && e["turn"]["status"] != "running")
                .count(),
            1
        );
        if mode == "no-history" {
            assert_eq!(state.summaries.load(Ordering::SeqCst), 0);
            let log = std::fs::read_to_string(
                paths
                    .data_dir
                    .join("logs/request-attempts")
                    .join(format!("{id}.jsonl")),
            )
            .unwrap();
            let terminal: Vec<Value> = log
                .lines()
                .map(|l| serde_json::from_str::<Value>(l).unwrap())
                .filter(|e| e["type"] == "request_turn_end")
                .collect();
            assert_eq!(terminal.len(), 1);
            assert_eq!(terminal[0]["failure"]["class"], "context_limit");
        } else {
            assert!(
                state.summaries.load(Ordering::SeqCst) > 0,
                "{mode}: recovery never started"
            );
        }
        if mode.starts_with("log-") {
            assert!(String::from_utf8_lossy(&output.stderr)
                .contains("Could not save request attempt log"));
        }
        if mode.contains("summary-failed") {
            assert!(result["error"].as_str().unwrap().contains("summary"));
            assert!(!result["error"]
                .as_str()
                .unwrap()
                .contains("Access is denied"));
        }
    }
}
