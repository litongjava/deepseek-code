use serde_json::{json, Value};
use std::{process::Stdio, sync::Arc, time::Duration};

#[tokio::test]
async fn standalone_session_start_watchpaths_dispatches_filechanged_and_exits_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir(&data).unwrap();
    let watched = dir.path().join("dynamic.env");
    let events = dir.path().join("events.jsonl");
    let debug_log = dir.path().join("debug.log");
    let script = dir.path().join("hook.cjs");
    std::fs::write(&script, r#"const fs=require('fs'),path=require('path');let s='';process.stdin.on('data',x=>s+=x);process.stdin.on('end',()=>{const v=JSON.parse(s);if(v.hook_event_name==='SessionStart'){console.log(JSON.stringify({hookSpecificOutput:{hookEventName:'SessionStart',watchPaths:[path.join(v.cwd,'dynamic.env')]}}));}else{fs.appendFileSync('events.jsonl',JSON.stringify(v)+'\n');console.log(JSON.stringify({continue:false,decision:'block',systemMessage:'file observer notice',hookSpecificOutput:{hookEventName:'FileChanged'}}));}});"#).unwrap();
    let command = format!("node \"{}\"", script.display());
    deepseek_code_core::data_layout::write_config_file(data.join("config/settings.json"),json!({"hooks":{
        "SessionStart":[{"hooks":[{"type":"command","command":command}]}],
        "FileChanged":[{"matcher":"^dynamic\\.env$","hooks":[{"type":"command","command":command}]}]
    }}).to_string()).unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let (seen, gate) = (entered.clone(), release.clone());
    let app = axum::Router::new().route("/v1/chat/completions",axum::routing::post(move || {
        let (seen,gate) = (seen.clone(),gate.clone());
        async move {
            seen.notify_one(); gate.notified().await;
            ([("content-type", "text/event-stream")], format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                json!({"choices":[{"index":0,"delta":{"content":"done"},"finish_reason":null}]}),
                json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})
            ))
        }
    }));
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let id = uuid::Uuid::new_v4().to_string();
    let child = tokio::process::Command::new(env!("CARGO_BIN_EXE_dcl"))
        .current_dir(dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .args([
            "--debug-file",
            debug_log.to_str().unwrap(),
            "--data-dir",
            data.to_str().unwrap(),
            "--base-url",
            &endpoint,
            "--api-format",
            "openai",
            "--api-key-env",
            "WATCH_FIXTURE_KEY",
            "--model",
            "fixture",
            "--print",
            "--session-id",
            &id,
            "--",
            "hello",
        ])
        .env("WATCH_FIXTURE_KEY", "fixture-only")
        .env("NO_PROXY", "127.0.0.1")
        .spawn()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), entered.notified())
        .await
        .unwrap();
    assert!(!events.exists());
    std::fs::write(&watched, "hello").unwrap();
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if std::fs::read_to_string(&events).is_ok_and(|s| s.ends_with('\n')) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    // Wait for the asynchronous hook result, not just the command's payload write.
    tokio::time::timeout(Duration::from_secs(15), async {
        while !std::fs::read_to_string(&debug_log)
            .is_ok_and(|text| text.contains("FileChanged hook: file observer notice"))
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    release.notify_one();
    let output = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("file observer notice"));
    let text = std::fs::read_to_string(&events).unwrap();
    let rows: Vec<Value> = text
        .lines()
        .map(|s| serde_json::from_str(s).unwrap())
        .collect();
    assert_eq!(rows.len(), 1);
    #[cfg(unix)]
    let cwd = dir.path().canonicalize().unwrap();
    #[cfg(windows)]
    let cwd = dir.path().to_path_buf();
    #[cfg(unix)]
    let watched = watched.canonicalize().unwrap();
    assert_eq!(
        rows[0],
        json!({"hook_event_name":"FileChanged","session_id":id,"cwd":cwd,"file_path":watched,"event":"add","transcript_path":rows[0]["transcript_path"]})
    );
    server.abort();
}
