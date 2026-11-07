use serde_json::{json, Value};
use std::{path::Path, process::Stdio, time::Duration};

fn configure(data: &Path, workspace: &Path, events: &[&str]) {
    std::fs::create_dir_all(data).unwrap();
    let script = workspace.join("capture.cjs");
    std::fs::write(&script, r#"const fs=require('fs');let s='';process.stdin.on('data',x=>s+=x);process.stdin.on('end',()=>{fs.appendFileSync('events.jsonl',JSON.stringify(JSON.parse(s))+'\n');console.log('{}')});"#).unwrap();
    let mut hooks = json!({});
    for event in events {
        hooks[*event] = json!([{"hooks":[{"type":"command","command":format!("node \"{}\"",script.display()),"once":true}]}]);
    }
    std::fs::write(
        data.join("settings.json"),
        json!({"hooks":hooks}).to_string(),
    )
    .unwrap();
}
fn rows(workspace: &Path) -> Vec<Value> {
    std::fs::read_to_string(workspace.join("events.jsonl"))
        .unwrap()
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect()
}

#[tokio::test]
async fn init_only_without_credentials_runs_setup_then_session_start_and_once_survives_resume() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    configure(&data, dir.path(), &["Setup", "SessionStart"]);
    let id = uuid::Uuid::new_v4().to_string();
    for resume in [false, true] {
        let output = tokio::time::timeout(
            Duration::from_secs(30),
            tokio::process::Command::new(env!("CARGO_BIN_EXE_dcl"))
                .current_dir(dir.path())
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .args([
                    "--data-dir",
                    data.to_str().unwrap(),
                    "--init-only",
                    if resume { "--resume" } else { "--session-id" },
                    &id,
                ])
                .env_remove("ANTHROPIC_API_KEY")
                .env_remove("OPENAI_API_KEY")
                .env_remove("DEEPSEEK_API_KEY")
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
    }
    let events = rows(dir.path());
    assert_eq!(events.len(), 2);
    #[cfg(unix)]
    let cwd = dir.path().canonicalize().unwrap();
    #[cfg(windows)]
    let cwd = dir.path();
    assert_eq!(
        events[0],
        json!({"hook_event_name":"Setup","session_id":id,"cwd":cwd,"transcript_path":events[0]["transcript_path"],"trigger":"init"})
    );
    assert!(Path::new(events[0]["transcript_path"].as_str().unwrap()).is_file());
    assert_eq!(events[1]["hook_event_name"], "SessionStart");
}

#[tokio::test]
async fn model_500_emits_one_stop_failure_and_loaded_instructions() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    configure(&data, dir.path(), &["StopFailure", "InstructionsLoaded"]);
    std::fs::write(dir.path().join("CLAUDE.md"), "Fixture instructions").unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(|| async {
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                [("retry-after", "0")],
                axum::Json(json!({"error":{"message":"lifecycle fixture","type":"server_error"}})),
            )
        }),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let id = uuid::Uuid::new_v4().to_string();
    let output = tokio::time::timeout(
        Duration::from_secs(90),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_dcl"))
            .current_dir(dir.path())
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
                "LIFECYCLE_FIXTURE_KEY",
                "--model",
                "fixture",
                "--print",
                "--session-id",
                &id,
                "--",
                "hello",
            ])
            .env("LIFECYCLE_FIXTURE_KEY", "fixture-only")
            .env("NO_PROXY", "127.0.0.1")
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!output.status.success());
    let events = rows(dir.path());
    let failures: Vec<_> = events
        .iter()
        .filter(|v| v["hook_event_name"] == "StopFailure")
        .collect();
    assert_eq!(failures.len(), 1, "{events:?}");
    let event = failures[0];
    assert!(event["error_details"].as_str().unwrap().contains("500"));
    #[cfg(unix)]
    let cwd = dir.path().canonicalize().unwrap();
    #[cfg(windows)]
    let cwd = dir.path();
    assert_eq!(
        *event,
        json!({"hook_event_name":"StopFailure","session_id":id,"cwd":cwd,"transcript_path":event["transcript_path"],"error":"server_error","error_details":event["error_details"]})
    );
    let instructions: Vec<_> = events
        .iter()
        .filter(|v| v["hook_event_name"] == "InstructionsLoaded")
        .collect();
    assert_eq!(instructions.len(), 1);
    assert_eq!(
        *instructions[0],
        json!({"hook_event_name":"InstructionsLoaded","session_id":id,"cwd":cwd,"transcript_path":event["transcript_path"],"file_path":std::fs::canonicalize(dir.path()).unwrap().join("CLAUDE.md"),"memory_type":"Project","load_reason":"session_start"})
    );
    server.abort();
}
