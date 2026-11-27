//! Real-process tests for the built-in resource monitor.
//!
//! Each test spawns the actual `dcl` binary against a local mock OpenAI
//! endpoint. The monitor is configured through an isolated `settings.json`, so
//! these tests prove the environment/configuration switches and the emitted
//! `resource_alert` stream event without depending on real host load.
use axum::{response::IntoResponse, routing::post, Json, Router};
use serde_json::{json, Value};
use std::{
    process::Stdio,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::process::Command;

fn openai_tool_sse(file_path: &str) -> String {
    let delta = json!({
        "tool_calls": [{
            "index": 0,
            "id": "read-1",
            "type": "function",
            "function": {"name": "Read", "arguments": json!({"file_path": file_path}).to_string()}
        }]
    });
    let first =
        json!({"model":"fixture-model","choices":[{"index":0,"delta":delta,"finish_reason":null}]});
    let second = json!({
        "model":"fixture-model",
        "choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],
        "usage":{"prompt_tokens":30,"completion_tokens":5,"total_tokens":35}
    });
    format!("data: {first}\n\ndata: {second}\n\ndata: [DONE]\n\n")
}

fn openai_text_sse(text: &str) -> String {
    let first = json!({"model":"fixture-model","choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]});
    let second = json!({
        "model":"fixture-model",
        "choices":[{"index":0,"delta":{},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":40,"completion_tokens":5,"total_tokens":45}
    });
    format!("data: {first}\n\ndata: {second}\n\ndata: [DONE]\n\n")
}

/// What the isolated `settings.json` says and what the child environment adds.
struct Case {
    settings_enabled: bool,
    env_override: Option<&'static str>,
    /// Delay the first response so the first monitor tick is guaranteed to
    /// complete before the follow-up request, even if it races the first one.
    first_response_delay: Duration,
}

struct Run {
    events: Vec<Value>,
    requests: Vec<Value>,
    stderr: String,
}

async fn run(case: Case) -> Run {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    let workspace = root.path().join("project");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();
    let fixture = workspace.join("fixture.txt");
    std::fs::write(&fixture, "fixture contents\n").unwrap();

    let recorded: Arc<Mutex<Vec<Value>>> = Arc::default();
    let recorded_clone = recorded.clone();
    let step = Arc::new(AtomicUsize::new(0));
    let step_clone = step.clone();
    let fixture_path = fixture.to_string_lossy().into_owned();
    let delay = case.first_response_delay;
    let app = Router::new().fallback(post(move |Json(body): Json<Value>| {
        let recorded = recorded_clone.clone();
        let step = step_clone.clone();
        let fixture_path = fixture_path.clone();
        async move {
            let index = step.fetch_add(1, Ordering::SeqCst);
            recorded.lock().unwrap().push(body);
            if index == 0 && !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let stream = if index == 0 {
                openai_tool_sse(&fixture_path)
            } else {
                openai_text_sse("FINAL_ANSWER")
            };
            ([("content-type", "text/event-stream")], stream).into_response()
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    deepseek_code_core::data_layout::write_config_file(
        data.join("config/providers.json"),
        json!([{
            "id": "fixture", "name": "Fixture", "base_url": base,
            "api_key": "fixture-only", "api_format": "openai", "enabled": true,
            "models": [{
                "id": "fixture-model", "name": "Fixture", "enabled": true,
                "context_window": 128000, "max_tokens": 4096,
                "supports_web_search": false, "supports_vision": false
            }]
        }])
        .to_string(),
    )
    .unwrap();
    deepseek_code_core::data_layout::write_config_file(
        data.join("config/settings.json"),
        json!({
            "resourceMonitor": {
                "enabled": case.settings_enabled,
                "intervalSecs": 2,
                // `available_percent()` is clamped to 0..=100, so a threshold
                // just above 100 guarantees the percentage test always passes.
                "memoryAvailablePercentThreshold": 100.1,
                // 100 TiB: safely inside JSON integer/u64 range and above any
                // real host's available memory.
                "memoryAvailableMinBytes": 109951162777600u64,
                "memoryConsecutive": 1,
                "cooldownSecs": 300
            }
        })
        .to_string(),
    )
    .unwrap();

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_dcl"));
    cmd.args([
        "--print",
        "--output-format",
        "stream-json",
        "--verbose",
        "--data-dir",
    ])
    .arg(&data)
    .arg("-C")
    .arg(&workspace)
    .args([
        "--provider",
        "fixture",
        "--model",
        "fixture-model",
        "--session-id",
        &uuid::Uuid::new_v4().to_string(),
        "--permission-mode",
        "ask",
        "read the fixture and answer",
    ])
    .stdin(Stdio::null())
    .kill_on_drop(true);
    for (key, _) in std::env::vars() {
        if key.starts_with("DEEPSEEK_CODE") {
            cmd.env_remove(key);
        }
    }
    if let Some(value) = case.env_override {
        cmd.env("DEEPSEEK_CODE_RESOURCE_MONITOR", value);
    }

    let output = tokio::time::timeout(Duration::from_secs(60), cmd.output())
        .await
        .expect("the CLI must finish")
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "the CLI failed: {stderr}\nstdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let events: Vec<Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.starts_with('{'))
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let requests = recorded.lock().unwrap().clone();
    server.abort();
    let _ = server.await;
    Run {
        events,
        requests,
        stderr,
    }
}

fn resource_alerts(run: &Run) -> Vec<&Value> {
    run.events
        .iter()
        .filter(|event| event["type"] == "resource_alert")
        .collect()
}

fn requests_with_notification(run: &Run) -> Vec<&Value> {
    run.requests
        .iter()
        .filter(|body| {
            let messages = body["messages"].to_string();
            messages.contains("<task-notification>")
                && messages.contains("<kind>system_resource</kind>")
        })
        .collect()
}

#[tokio::test]
async fn print_stream_json_reports_memory_alert_and_injects_notification() {
    let run = run(Case {
        settings_enabled: true,
        env_override: None,
        first_response_delay: Duration::from_millis(2600),
    })
    .await;

    let alerts = resource_alerts(&run);
    assert_eq!(
        alerts.len(),
        1,
        "exactly one resource_alert event: {}\n{}",
        run.stderr,
        serde_json::to_string_pretty(&run.events).unwrap()
    );
    assert_eq!(alerts[0]["kind"], "memory");
    assert!(
        alerts[0]["summary"]
            .as_str()
            .is_some_and(|summary| !summary.is_empty()),
        "summary must be non-empty: {}",
        alerts[0]
    );
    assert!(
        alerts[0]["details"]
            .as_str()
            .is_some_and(|details| !details.is_empty()),
        "details must be non-empty: {}",
        alerts[0]
    );

    assert!(
        !requests_with_notification(&run).is_empty(),
        "the model request must carry the task notification"
    );
}

#[tokio::test]
async fn disabled_setting_stops_sampling_and_injection() {
    let run = run(Case {
        settings_enabled: false,
        env_override: None,
        first_response_delay: Duration::ZERO,
    })
    .await;

    assert!(
        resource_alerts(&run).is_empty(),
        "a disabled monitor must not emit resource alerts: {}",
        serde_json::to_string_pretty(&run.events).unwrap()
    );
    assert!(
        requests_with_notification(&run).is_empty(),
        "a disabled monitor must not inject a notification"
    );
}

#[tokio::test]
async fn environment_override_disables_even_when_settings_enable_it() {
    let run = run(Case {
        settings_enabled: true,
        env_override: Some("0"),
        first_response_delay: Duration::ZERO,
    })
    .await;

    assert!(
        resource_alerts(&run).is_empty(),
        "the environment must override the settings file: {}",
        serde_json::to_string_pretty(&run.events).unwrap()
    );
    assert!(
        requests_with_notification(&run).is_empty(),
        "a disabled monitor must not inject a notification"
    );
}
