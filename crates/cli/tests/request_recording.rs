//! Real CLI process switch resolution and read-only record inspection.
use axum::{routing::post, Router};
use deepseek_code_core::runtime::{RuntimePaths, SessionStore};
use serde_json::json;
use std::{path::Path, process::Stdio};
use tokio::process::Command;
fn cli(root: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_dcl"));
    c.arg("--data-dir")
        .arg(root)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    for (key, _) in std::env::vars() {
        if key.starts_with("DEEPSEEK_CODE") {
            c.env_remove(key);
        }
    }
    c
}
#[tokio::test]
async fn actual_cli_switches_and_requests_listing() {
    let app = Router::new().fallback(post(|| async {
        ([("content-type","text/event-stream")], "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"RECORD_REPLY\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let cases: &[(&[&str], Option<&str>, Option<bool>, bool)] = &[
        (&[], None, None, false),
        (&["--record-requests"], None, None, true),
        (&[], Some("1"), None, true),
        (&[], Some("true"), None, true),
        (&[], None, Some(true), true),
        (&["--full-access"], None, None, true),
        (&["--yolo"], None, None, true),
        (&["--yolo"], Some("0"), Some(true), false),
        (&["--full-access"], Some("false"), None, false),
        (&["--yolo"], None, Some(false), false),
        (&[], Some("true"), Some(false), true),
        (
            &["--record-requests", "--yolo"],
            Some("0"),
            Some(false),
            true,
        ),
    ];
    for (index, (args, env, setting, expected)) in cases.iter().enumerate() {
        let temp = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
        let store = SessionStore::new(paths.open_database().unwrap());
        let id = store
            .create(temp.path(), "fixture-model", "fixture")
            .unwrap();
        std::fs::write(paths.data_dir.join("providers.json"),json!([{
            "id":"fixture","name":"fixture","base_url":base,"api_key":"sk-test-only","api_format":"openai","enabled":true,
            "models":[{"id":"fixture-model","name":"fixture","enabled":true,"context_window":128000,"max_tokens":4096,"supports_web_search":false}]
        }]).to_string()).unwrap();
        if let Some(setting) = setting {
            std::fs::write(
                paths.data_dir.join("settings.json"),
                json!({"requestRecording":{"enabled":setting}}).to_string(),
            )
            .unwrap();
        }
        let mut command = cli(&paths.data_dir);
        command
            .args(*args)
            .args([
                "--print",
                "--provider",
                "fixture",
                "--model",
                "fixture-model",
                "--resume",
                &id,
                "hello recording",
            ])
            .arg("--path")
            .arg(temp.path());
        if let Some(value) = env {
            command.env("DEEPSEEK_CODE_RECORD_REQUESTS", value);
        }
        let output = tokio::time::timeout(std::time::Duration::from_secs(45), command.output())
            .await
            .unwrap()
            .unwrap();
        assert!(
            output.status.success(),
            "case {index}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("RECORD_REPLY"));
        assert_eq!(
            paths.data_dir.join("request-records").exists(),
            *expected,
            "case {index}"
        );
        let result = cli(&paths.data_dir)
            .args(["requests", &id])
            .output()
            .await
            .unwrap();
        assert!(result.status.success());
        let listing = String::from_utf8(result.stdout).unwrap();
        assert!(listing.contains(&id));
        if index == 1 {
            let by_env = Command::new(env!("CARGO_BIN_EXE_dcl"))
                .env("DEEPSEEK_CODE_DATA_DIR", &paths.data_dir)
                .args(["requests", &id])
                .stdin(Stdio::null())
                .output()
                .await
                .unwrap();
            assert!(by_env.status.success());
            assert_eq!(String::from_utf8(by_env.stdout).unwrap(), listing);
        }
        if *expected {
            assert!(listing.contains("purpose=\"main\""));
            assert!(listing.contains("fixture-model"));
            assert!(listing.contains("HTTP=200"));
            assert!(listing.contains("bytes:"));
            for suffix in ["request.json", "headers.json", "sse.txt", "meta.json"] {
                assert!(listing.contains(suffix));
            }
        } else {
            assert!(listing.contains("No request records"));
        }
    }
    server.abort();
}

#[tokio::test]
async fn requests_help_and_absence_are_read_only_and_reject_traversal() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("missing-data");
    for args in [["requests", "missing-session"], ["requests", "--help"]] {
        let output = cli(&data).args(args).output().await.unwrap();
        assert!(output.status.success());
        assert!(!data.exists());
    }
    let output = cli(&data)
        .args(["requests", "../escape"])
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    assert!(!data.exists());
    let output = cli(&data).arg("--help").output().await.unwrap();
    let help = String::from_utf8(output.stdout).unwrap();
    assert!(help.contains("requests"));
    assert!(help.contains("--record-requests"));
}
