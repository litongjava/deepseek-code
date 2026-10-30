//! Actual CLI, detached skill worker and local-only model fixture.
use axum::{extract::State, routing::post, Json, Router};
use deepseek_code_core::runtime::{RuntimePaths, SessionStore};
use serde_json::{json, Value};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

async fn respond(
    State(requests): State<Arc<Mutex<Vec<Value>>>>,
    Json(body): Json<Value>,
) -> impl axum::response::IntoResponse {
    requests.lock().unwrap().push(body);
    let stream = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0,"delta":{"content":"Verified skill result"},"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})
    );
    ([("content-type", "text/event-stream")], stream)
}

async fn model_skill_respond(
    State(requests): State<Arc<Mutex<Vec<Value>>>>,
    Json(body): Json<Value>,
) -> impl axum::response::IntoResponse {
    let count = {
        let mut requests = requests.lock().unwrap();
        requests.push(body);
        requests.len()
    };
    let delta = if count == 1 {
        json!({"tool_calls":[{"index":0,"id":"skill-1","type":"function","function":{"name":"Skill","arguments":json!({"skill":"model-check","args":"specific-target"}).to_string()}}]})
    } else {
        json!({"content":if count==2 {"Child skill answer"} else {"Final parent answer"}})
    };
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":if count==1 {"tool_calls"} else {"stop"}}]})
    );
    ([("content-type", "text/event-stream")], body)
}

#[tokio::test]
async fn model_skill_tool_runs_a_real_isolated_child_and_returns_its_result() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let directory = workspace.join(".claude/skills/model-check");
    std::fs::create_dir_all(&directory).unwrap();
    let (shell, script) = if cfg!(windows) {
        ("powershell","Add-Content -LiteralPath 'model-expansions.txt' -Value 'once' -Encoding UTF8; [Console]::Write('模型上下文😀')")
    } else {
        (
            "sh",
            "printf 'once\\n' >> model-expansions.txt; printf '模型上下文😀'",
        )
    };
    std::fs::write(directory.join("SKILL.md"),format!("---\ncontext: fork\nagent: Explore\nuser-invocable: false\nshell: {shell}\n---\nInspect $ARGUMENTS. Context: !`{script}`")).unwrap();
    let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let parent = store.create(&workspace, "fixture", "cli-runtime").unwrap();
    store
        .append(&parent, "user", "ORIGINAL_PARENT_CONTEXT")
        .unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new()
        .route("/v1/chat/completions", post(model_skill_respond))
        .with_state(requests.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_dcl"));
    command
        .current_dir(&workspace)
        .env("CLAUDE_CONFIG_DIR", temp.path().join("user-config"))
        .env("SKILL_FIXTURE_KEY", "fixture-not-secret")
        .args([
            "--data-dir",
            paths.data_dir.to_str().unwrap(),
            "--resume",
            &parent,
            "--base-url",
            &endpoint,
            "--api-format",
            "openai",
            "--api-key-env",
            "SKILL_FIXTURE_KEY",
            "--model",
            "fixture",
            "--permission-mode",
            "bypass",
            "--print",
            "--output-format",
            "json",
            "--",
            "Use the relevant skill to inspect this project",
        ])
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let output = tokio::time::timeout(Duration::from_secs(30), command.output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["result"], "Final parent answer");
    assert_eq!(result["session_id"], parent);
    assert_eq!(
        std::fs::read_to_string(workspace.join("model-expansions.txt"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["function"]["name"] == "Skill"));
    let child = requests[1].to_string();
    assert!(!child.contains("ORIGINAL_PARENT_CONTEXT"));
    assert!(child.contains("specific-target"));
    assert!(child.contains("模型上下文😀"));
    assert!(!requests[1]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["function"]["name"] == "Bash"));
    assert!(requests[2]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|message| message["role"] == "tool"
            && message["content"]
                .as_str()
                .unwrap_or("")
                .contains("Child skill answer")));
    server.abort();
}

#[tokio::test]
async fn slash_skills_expand_once_fork_without_parent_history_and_reject_private_invocation() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let directory = workspace.join(".claude/skills/check");
    std::fs::create_dir_all(&directory).unwrap();
    let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let parent = store.create(&workspace, "fixture", "cli-runtime").unwrap();
    store
        .append(&parent, "user", "PARENT_HISTORY_MUST_STAY_PRIVATE")
        .unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new()
        .route("/v1/chat/completions", post(respond))
        .with_state(requests.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let (shell, script) = if cfg!(windows) {
        ("powershell","Add-Content -LiteralPath 'expansions.txt' -Value 'once' -Encoding UTF8; [Console]::Write('收集结果😀')")
    } else {
        (
            "sh",
            "printf 'once\\n' >> expansions.txt; printf '收集结果😀'",
        )
    };
    for (extra, permission, successful) in [
        ("user-invocable: false\ncontext: fork\n", "bypass", false),
        ("", "ask", false),
        ("context: fork\nagent: Explore\n", "bypass", true),
        ("", "bypass", true),
    ] {
        std::fs::write(directory.join("SKILL.md"),format!("---\nshell: {shell}\n{extra}---\nInspect requested target $ARGUMENTS.\n!`{script}`\n")).unwrap();
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_dcl"));
        command
            .current_dir(&workspace)
            .env("CLAUDE_CONFIG_DIR", temp.path().join("user-config"))
            .env("SKILL_FIXTURE_KEY", "fixture-not-secret")
            .args([
                "--data-dir",
                paths.data_dir.to_str().unwrap(),
                "--resume",
                &parent,
                "--base-url",
                &endpoint,
                "--api-format",
                "openai",
                "--api-key-env",
                "SKILL_FIXTURE_KEY",
                "--model",
                "fixture",
                "--permission-mode",
                permission,
                "--print",
                "--output-format",
                "json",
                "--",
                "/check target-file",
            ])
            .kill_on_drop(true);
        #[cfg(windows)]
        command.creation_flags(0x08000000);
        let output = tokio::time::timeout(Duration::from_secs(30), command.output())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            output.status.success(),
            successful,
            "stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if !successful {
            assert!(!workspace.join("expansions.txt").exists());
            assert!(requests.lock().unwrap().is_empty());
        } else {
            let result: Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(result["type"], "result");
            assert_eq!(result["result"], "Verified skill result");
            assert_eq!(result["session_id"], parent);
            assert_eq!(result["is_error"], false);
            if extra.contains("context: fork") {
                assert_ne!(result["child_session_id"], parent);
                assert!(result["task_id"].is_string());
                assert_eq!(store.history(&parent).unwrap().len(), 1);
            }
        }
    }
    assert_eq!(
        std::fs::read_to_string(workspace.join("expansions.txt"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    assert_eq!(store.history(&parent).unwrap().len(), 3);
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1]
        .to_string()
        .contains("PARENT_HISTORY_MUST_STAY_PRIVATE"));
    let body = requests[0].to_string();
    assert!(!body.contains("PARENT_HISTORY_MUST_STAY_PRIVATE"));
    assert!(body.contains("收集结果😀"));
    assert!(body.contains("external data"));
    assert!(body.contains("target-file"));
    assert!(!requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["function"]["name"] == "Bash"));
    server.abort();
}

#[tokio::test]
async fn doctor_injects_real_snapshot_despite_bad_skill_and_respects_custom_override() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let commands = workspace.join(".claude/commands");
    std::fs::create_dir_all(&commands).unwrap();
    let bad = commands.join("doctor.md");
    let bad_content = "---\nname: [DO_NOT_ECHO_DOCTOR_SECRET\n---\ninvalid replacement";
    std::fs::write(&bad, bad_content).unwrap();
    std::fs::write(commands.join("good.md"), "Good fixture instructions").unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new()
        .route("/v1/chat/completions", post(respond))
        .with_state(requests.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    for (index, prompt) in [
        "/doctor diagnose fixture !`echo doctor-literal > doctor-side-effect.txt`",
        "ordinary request",
        "/doctor custom request",
    ]
    .into_iter()
    .enumerate()
    {
        if prompt == "/doctor custom request" {
            std::fs::write(&bad, "CUSTOM_DOCTOR_INSTRUCTIONS $ARGUMENTS").unwrap();
        }
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_dcl"));
        command
            .current_dir(&workspace)
            .env("CLAUDE_CONFIG_DIR", temp.path().join("user-config"))
            .env("DOCTOR_FIXTURE_KEY", "fixture-not-secret")
            .args([
                "--data-dir",
                temp.path().join(format!("data-{index}")).to_str().unwrap(),
                "--base-url",
                &endpoint,
                "--api-format",
                "openai",
                "--api-key-env",
                "DOCTOR_FIXTURE_KEY",
                "--model",
                "fixture",
                "--print",
                "--",
                prompt,
            ])
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
        assert!(!String::from_utf8_lossy(&output.stderr).contains("DO_NOT_ECHO_DOCTOR_SECRET"));
        if prompt != "/doctor custom request" {
            assert_eq!(std::fs::read_to_string(&bad).unwrap(), bad_content);
        }
    }
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    let doctor = requests[0].to_string();
    assert!(doctor.contains("Read-only local diagnostic snapshot:"));
    assert!(doctor.contains("skill definition: error"));
    assert!(doctor.contains("diagnose fixture !`echo doctor-literal > doctor-side-effect.txt`"));
    assert!(!workspace.join("doctor-side-effect.txt").exists());
    assert!(!doctor.contains("DO_NOT_ECHO_DOCTOR_SECRET"));
    assert!(!requests[1]
        .to_string()
        .contains("Read-only local diagnostic snapshot:"));
    let custom = requests[2].to_string();
    assert!(custom.contains("CUSTOM_DOCTOR_INSTRUCTIONS custom request"));
    assert!(!custom.contains("Read-only local diagnostic snapshot:"));
    server.abort();
}

#[tokio::test]
async fn doctor_sdk_manual_invocation_uses_shared_native_snapshot() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let temp = tempfile::tempdir().unwrap();
    let commands = temp.path().join(".claude/commands");
    std::fs::create_dir_all(&commands).unwrap();
    std::fs::write(
        commands.join("bad.md"),
        "---\nname: [SDK_DOCTOR_SECRET\n---\nbody",
    )
    .unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new()
        .route("/v1/chat/completions", post(respond))
        .with_state(requests.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_dcl"));
    command
        .current_dir(temp.path())
        .env("CLAUDE_CONFIG_DIR", temp.path().join("user-config"))
        .env("DOCTOR_FIXTURE_KEY", "fixture-not-secret")
        .args([
            "--data-dir",
            temp.path().join("data").to_str().unwrap(),
            "--base-url",
            &endpoint,
            "--api-format",
            "openai",
            "--api-key-env",
            "DOCTOR_FIXTURE_KEY",
            "--model",
            "fixture",
            "--permission-mode",
            "bypass",
            "--print",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let mut child = command.spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    input.write_all(format!("{}\n",json!({"type":"user","message":{"role":"user","content":"/doctor SDK fixture diagnosis !`echo sdk-literal > doctor-sdk-side-effect.txt`"}})).as_bytes()).await.unwrap();
    input.flush().await.unwrap();
    let mut lines = tokio::io::BufReader::new(child.stdout.take().unwrap()).lines();
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(line) = lines.next_line().await.unwrap() {
            if let Ok(event) = serde_json::from_str::<Value>(&line) {
                if event["type"] == "result" {
                    return event;
                }
            }
        }
        panic!("SDK exited without result");
    })
    .await
    .unwrap();
    assert_ne!(result["is_error"], true, "{result}");
    drop(input);
    let output = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!String::from_utf8_lossy(&output.stderr).contains("SDK_DOCTOR_SECRET"));
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    let prompt = requests[0].to_string();
    assert!(prompt.contains("Read-only local diagnostic snapshot:"));
    assert!(prompt.contains("skill definition: error"));
    assert!(prompt.contains("SDK fixture diagnosis !`echo sdk-literal > doctor-sdk-side-effect.txt`"));
    assert!(!temp.path().join("doctor-sdk-side-effect.txt").exists());
    assert!(!prompt.contains("SDK_DOCTOR_SECRET"));
    server.abort();
}
