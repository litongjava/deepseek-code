//! Exercise the real process/worker boundary and model-driven Skill -> Read.
use axum::{extract::State, routing::post, Json, Router};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Default)]
struct Capture {
    calls: usize,
    root: Option<PathBuf>,
    read_ok: bool,
}

fn resource_root(value: &Value) -> Option<PathBuf> {
    match value {
        Value::String(text) => {
            if let Ok(inner) = serde_json::from_str::<Value>(text) {
                if !inner.is_string() {
                    if let Some(root) = resource_root(&inner) {
                        return Some(root);
                    }
                }
            }
            text.lines().find_map(|line| {
                line.strip_prefix("Base directory for this skill: ")
                    .map(PathBuf::from)
            })
        }
        Value::Array(items) => items.iter().find_map(resource_root),
        Value::Object(items) => items.values().find_map(resource_root),
        _ => None,
    }
}

async fn respond(
    State(state): State<Arc<Mutex<Capture>>>,
    Json(body): Json<Value>,
) -> impl axum::response::IntoResponse {
    let mut capture = state.lock().unwrap();
    capture.calls += 1;
    let request = body.to_string();
    let read = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["role"] == "tool" && m["tool_call_id"] == "asset-read");
    let delta = if read {
        capture.read_ok = request.contains("Begin with the question the chart has to answer")
            && !request.contains("Path is outside allowed directories");
        json!({"content":"Resource fixture completed."})
    } else if let Some(root) = resource_root(&body["messages"]) {
        assert!(root.join("scripts/check_contrast.py").is_file());
        capture.root = Some(root.clone());
        json!({"tool_calls":[{"index":0,"id":"asset-read","type":"function","function":{"name":"Read","arguments":json!({"file_path":root.join("scripts/check_contrast.py"),"offset":1,"limit":12}).to_string()}}]})
    } else if request.contains("CUSTOM_DATAVIZ") {
        json!({"content":"Custom override honored."})
    } else {
        json!({"tool_calls":[{"index":0,"id":"activate-skill","type":"function","function":{"name":"Skill","arguments":json!({"skill":"dataviz","args":"Use local resources only"}).to_string()}}]})
    };
    let reason = if delta.get("tool_calls").is_some() {
        "tool_calls"
    } else {
        "stop"
    };
    (
        [("content-type", "text/event-stream")],
        format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"index":0,"delta":delta,"finish_reason":reason}]})
        ),
    )
}

#[tokio::test]
async fn manual_and_model_invocations_read_resources_and_preserve_custom_overrides() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    for (index, prompt) in [
        "/dataviz literal !`echo never-execute` 中文😀",
        "Use the dataviz skill for this fixture",
        "/dataviz custom",
    ]
    .into_iter()
    .enumerate()
    {
        if index == 2 {
            let dir = workspace.join(".claude/commands");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("dataviz.md"), "CUSTOM_DATAVIZ $ARGUMENTS").unwrap();
        }
        let capture = Arc::new(Mutex::new(Capture::default()));
        let data = temp.path().join(format!("data-{index}"));
        if index < 2 {
            // Built-in skills default to off; this fixture exercises dataviz.
            std::fs::create_dir_all(data.join("config")).unwrap();
            std::fs::write(
                data.join("config/skill-states.json"),
                r#"{"skills":{"dataviz":"user"}}"#,
            )
            .unwrap();
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new()
            .route("/v1/chat/completions", post(respond))
            .with_state(capture.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_dcl"));
        command
            .current_dir(&workspace)
            .env("CLAUDE_CONFIG_DIR", temp.path().join("user-config"))
            .env("BUNDLE_FIXTURE_KEY", "fixture-not-secret")
            .args([
                "--data-dir",
                data.to_str().unwrap(),
                "--base-url",
                &endpoint,
                "--api-format",
                "openai",
                "--api-key-env",
                "BUNDLE_FIXTURE_KEY",
                "--model",
                "fixture",
                "--dangerously-skip-permissions",
                "--print",
                "--output-format",
                "json",
                "--max-turns",
                "5",
                "--",
                prompt,
            ])
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(0x08000000);
        let output = tokio::time::timeout(Duration::from_secs(40), command.output())
            .await
            .unwrap()
            .unwrap();
        assert!(
            output.status.success(),
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let capture = capture.lock().unwrap();
        if index < 2 {
            assert!(capture.read_ok);
            assert_eq!(capture.calls, if index == 0 { 2 } else { 3 });
            assert!(
                capture.root.as_ref().unwrap().exists(),
                "the installed resource directory persists after the worker exits"
            );
        } else {
            assert_eq!(capture.calls, 1);
            assert!(capture.root.is_none());
        }
        server.abort();
    }
}
