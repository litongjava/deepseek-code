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
    let child_tools = requests[1]["tools"].as_array().unwrap();
    assert!(child_tools.iter().all(|tool| ![
        "Write",
        "Edit",
        "MultiEdit",
        "NotebookEdit"
    ]
    .contains(&tool["function"]["name"].as_str().unwrap_or(""))));
    // The read-only gate is tested in read_only_agent_runs_read_only_bash_and_refuses_the_rest.
    assert!(child_tools
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
                "--max-tokens",
                "211",
                "--system-prompt",
                "I4_BASE_SKILL_PROMPT",
                "--append-system-prompt",
                "I4_APPEND_SKILL_PROMPT",
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
                let rows = store
                    .model_history(
                        result["child_session_id"].as_str().unwrap(),
                        &deepseek_code_core::native_engine::provider_manager::ApiFormat::OpenAI,
                    )
                    .unwrap();
                assert_eq!(
                    rows.iter().find(|r| r["role"] == "user").unwrap()["origin"],
                    "generated"
                );
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
    assert!(requests.iter().all(|request| request["max_tokens"] == 211));
    assert!(requests[1]
        .to_string()
        .contains("PARENT_HISTORY_MUST_STAY_PRIVATE"));
    let body = requests[0].to_string();
    assert!(!body.contains("PARENT_HISTORY_MUST_STAY_PRIVATE"));
    assert!(body.contains("收集结果😀"));
    assert!(body.contains("external data"));
    assert!(body.contains("target-file"));
    for marker in [
        "I4_BASE_SKILL_PROMPT",
        "I4_APPEND_SKILL_PROMPT",
        "收集结果😀",
    ] {
        assert_eq!(body.matches(marker).count(), 1, "{marker}");
    }
    let child_tools = requests[0]["tools"].as_array().unwrap();
    assert!(child_tools.iter().all(|tool| ![
        "Write",
        "Edit",
        "MultiEdit",
        "NotebookEdit"
    ]
    .contains(&tool["function"]["name"].as_str().unwrap_or(""))));
    // The read-only gate is tested in read_only_agent_runs_read_only_bash_and_refuses_the_rest.
    assert!(child_tools
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
        // Built-in skills default to off; /doctor is exercised here because the
        // user enabled it.
        let data = temp.path().join(format!("data-{index}"));
        std::fs::create_dir_all(data.join("config")).unwrap();
        std::fs::write(
            data.join("config/skill-states.json"),
            r#"{"skills":{"doctor":"user"}}"#,
        )
        .unwrap();
        let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_dcl"));
        command
            .current_dir(&workspace)
            .env("CLAUDE_CONFIG_DIR", temp.path().join("user-config"))
            .env("DOCTOR_FIXTURE_KEY", "fixture-not-secret")
            .args([
                "--data-dir",
                data.to_str().unwrap(),
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
    // Built-in skills default to off; /doctor is exercised because the user
    // enabled it.
    let data = temp.path().join("data");
    std::fs::create_dir_all(data.join("config")).unwrap();
    std::fs::write(
        data.join("config/skill-states.json"),
        r#"{"skills":{"doctor":"user"}}"#,
    )
    .unwrap();
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_dcl"));
    command
        .current_dir(temp.path())
        .env("CLAUDE_CONFIG_DIR", temp.path().join("user-config"))
        .env("DOCTOR_FIXTURE_KEY", "fixture-not-secret")
        .args([
            "--data-dir",
            data.to_str().unwrap(),
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

async fn data_dir_skill_respond(
    State(requests): State<Arc<Mutex<Vec<Value>>>>,
    Json(body): Json<Value>,
) -> impl axum::response::IntoResponse {
    let count = {
        let mut requests = requests.lock().unwrap();
        requests.push(body);
        requests.len()
    };
    let delta = if count == 1 {
        json!({"tool_calls":[{"index":0,"id":"skill-1","type":"function","function":{"name":"Skill","arguments":json!({"skill":"data-dir-listed","args":"the fixture"}).to_string()}}]})
    } else if count == 2 {
        json!({"content":"Data directory skill executed"})
    } else {
        json!({"content":"Final data directory answer"})
    };
    let body = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":if count == 1 {"tool_calls"} else {"stop"}}]})
    );
    ([("content-type", "text/event-stream")], body)
}

#[tokio::test]
async fn data_dir_skill_is_listed_for_the_model_and_invoked_with_an_empty_claude_config() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let data = temp.path().join("data");
    let directory = data.join("skills/Data Dir Listed");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join("SKILL.md"),
        "---\nname: Data Dir Listed\ndescription: Listed from the data directory\n---\nRead references/setup.md, then inspect $ARGUMENTS",
    )
    .unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new()
        .route("/v1/chat/completions", post(data_dir_skill_respond))
        .with_state(requests.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_dcl"));
    command
        .current_dir(&workspace)
        // Deliberately absent: the skill must come from the data directory.
        .env("CLAUDE_CONFIG_DIR", temp.path().join("empty-user-config"))
        .env("SKILL_FIXTURE_KEY", "fixture-not-secret")
        .args([
            "--data-dir",
            data.to_str().unwrap(),
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
            "Use the available data directory skill",
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
    assert_eq!(result["result"], "Data directory skill executed");
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2, "{requests:?}");
    let listing = requests[0].to_string();
    assert!(listing.contains("Available skills"), "{listing}");
    assert_eq!(listing.matches("data-dir-listed").count(), 1, "{listing}");
    assert!(!listing.contains("Data-Dir-Listed"), "{listing}");
    let executed = requests[1].to_string();
    let rendered = directory.to_string_lossy().replace('\\', "/");
    assert!(executed.contains(&rendered), "{executed}");
    assert!(executed.contains("the fixture"), "{executed}");
    server.abort();
}

#[test]
fn skills_list_and_help_expose_the_embedded_task_template() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let run = |args: &[&str]| {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_dcl"))
            .current_dir(temp.path())
            .args(["--data-dir", data.to_str().unwrap(), "skills"])
            .args(args)
            .env_remove("DEEPSEEK_CODE_DATA_DIR")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };

    let skills: Vec<Value> = serde_json::from_str(&run(&["list"])).unwrap();
    let matching: Vec<_> = skills
        .iter()
        .filter(|skill| skill["name"] == "task-template")
        .collect();
    assert_eq!(matching.len(), 1);
    let skill = matching[0];
    assert_eq!(skill["source"], "bundled");
    // Built-in skills ship disabled by default but stay listed with their state,
    // and `enabled` agrees with that state.
    assert_eq!(skill["state"], "off");
    assert_eq!(skill["enabled"], false);
    assert_eq!(skill["files"][0]["name"], "SKILL.md");

    let enabled = run(&["state", "task-template", "user"]);
    assert!(enabled.contains("task-template  user"), "{enabled}");
    let skills: Vec<Value> = serde_json::from_str(&run(&["list"])).unwrap();
    let skill = skills
        .iter()
        .find(|skill| skill["name"] == "task-template")
        .expect("task-template stays listed after enabling");
    assert_eq!(skill["state"], "user");

    let help = run(&["help", "task-template"]);
    assert_eq!(
        help.trim_end(),
        skill["content"].as_str().unwrap().trim_end()
    );
    assert!(help.contains("name: task-template"));
    assert!(help.contains("requirement → evidence → unverified"));
    let installed = data.join("skills/bundled/task-template/SKILL.md");
    assert!(installed.exists());
    let document = std::fs::read_to_string(installed).unwrap();
    assert!(document.contains("user-invocable: true"));
    assert!(document.contains("disable-model-invocation: false"));
}

#[test]
fn skills_list_migrates_the_pristine_placeholder_and_lists_one_skill_creator() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let stub = data.join("skills/bundled/skill-creator");
    std::fs::create_dir_all(stub.join("agents")).unwrap();
    std::fs::write(
        stub.join("SKILL.md"),
        r#"---
name: skill-creator
description: A toolkit for creating, evaluating, and improving Claude skills
whenToUse: When you want to build a new skill, evaluate an existing one, or generate synthetic training data for skills
---

# Skill Creator

A comprehensive toolkit for creating, evaluating, and improving Claude skills.

## Usage

This skill provides utilities for:
- Creating new skills from templates
- Evaluating skill quality
- Generating training data
- Packaging skills for distribution

## Files

- `agents/` - Agent definitions for different evaluation tasks
- `scripts/` - Utility scripts for skill development
- `eval-viewer/` - Evaluation result visualization
- `references/` - Schema definitions and documentation
"#,
    )
    .unwrap();
    std::fs::write(
        stub.join("agents/analyzer.md"),
        "# Analyzer\n\nAnalyzes skill structure and quality.",
    )
    .unwrap();
    std::fs::write(
        stub.join("agents/grader.md"),
        "# Grader\n\nGrades skill performance.",
    )
    .unwrap();
    std::fs::create_dir_all(data.join("skills/bundled/legacy-review")).unwrap();
    std::fs::write(
        data.join("skills/bundled/legacy-review/SKILL.md"),
        "user sibling",
    )
    .unwrap();

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_dcl"))
        .args(["--data-dir", data.to_str().unwrap(), "skills", "list"])
        .env_remove("DEEPSEEK_CODE_DATA_DIR")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let skills: Vec<Value> = serde_json::from_slice(&output.stdout).unwrap();
    let creators: Vec<_> = skills
        .iter()
        .filter(|skill| skill["name"] == "skill-creator")
        .collect();
    assert_eq!(creators.len(), 1, "{skills:?}");
    assert_eq!(creators[0]["source"], "bundled");
    let files = creators[0]["files"].as_array().unwrap();
    assert!(files.iter().any(|file| file["name"] == "scripts"));
    assert!(files.iter().any(|file| file["name"] == "license.txt"));
    assert!(stub.join("SKILL.md").exists());
    assert!(!stub.join("agents").exists());
    // The edited sibling belongs to the user and survives; pristine generated
    // legacy skills are neither installed nor listed any more.
    assert_eq!(
        std::fs::read_to_string(data.join("skills/bundled/legacy-review/SKILL.md")).unwrap(),
        "user sibling"
    );
    for name in ["build", "commit", "document", "test"] {
        assert!(!data.join(format!("skills/bundled/legacy-{name}")).exists());
    }

    let help = std::process::Command::new(env!("CARGO_BIN_EXE_dcl"))
        .args([
            "--data-dir",
            data.to_str().unwrap(),
            "skills",
            "help",
            "skill-creator",
        ])
        .env_remove("DEEPSEEK_CODE_DATA_DIR")
        .output()
        .unwrap();
    assert!(
        help.status.success(),
        "{}",
        String::from_utf8_lossy(&help.stderr)
    );
    let text = String::from_utf8_lossy(&help.stdout);
    assert!(text.contains("name: skill-creator"));
    assert!(text.contains("DeepSeek Code"));
    assert!(text.contains("NOTICE.md"));
    assert!(!text.contains("$CODEX_HOME"));
}

fn run_skills(data: &std::path::Path, args: &[&str]) -> Value {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_dcl"))
        .args(["--data-dir", data.to_str().unwrap(), "skills"])
        .args(args)
        .env_remove("DEEPSEEK_CODE_DATA_DIR")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn listed_state(data: &std::path::Path, name: &str) -> String {
    let skills = run_skills(data, &["list"]);
    let skill = skills
        .as_array()
        .unwrap()
        .iter()
        .find(|skill| skill["name"] == name)
        .unwrap_or_else(|| panic!("{name} missing from `dcl skills list`"))
        .clone();
    let state = skill["state"].as_str().unwrap().to_owned();
    // `enabled` must agree with the effective state.
    assert_eq!(skill["enabled"], state != "off", "{name}: {skill}");
    state
}

#[test]
fn disabled_builtins_keep_their_state_across_reinstall() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");

    // A fresh data directory installs every built-in but only skill-creator is
    // enabled; the rest are still listed with their state.
    assert_eq!(listed_state(&data, "skill-creator"), "user");
    for name in ["task-template", "docx", "pdf", "commit"] {
        assert_eq!(listed_state(&data, name), "off", "{name}");
        assert!(
            data.join(format!("skills/bundled/{name}/SKILL.md")).is_file(),
            "{name} must stay installed"
        );
    }

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_dcl"))
        .args([
            "--data-dir",
            data.to_str().unwrap(),
            "skills",
            "state",
            "docx",
            "user",
        ])
        .env_remove("DEEPSEEK_CODE_DATA_DIR")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(listed_state(&data, "docx"), "user");

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_dcl"))
        .args([
            "--data-dir",
            data.to_str().unwrap(),
            "skills",
            "state",
            "skill-creator",
            "off",
        ])
        .env_remove("DEEPSEEK_CODE_DATA_DIR")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(listed_state(&data, "skill-creator"), "off");

    // An upgrade rewrites the installed tree; the user's own choices stay.
    std::fs::remove_dir_all(data.join("skills/bundled")).unwrap();
    assert_eq!(listed_state(&data, "docx"), "user");
    assert_eq!(listed_state(&data, "skill-creator"), "off");
    assert_eq!(listed_state(&data, "task-template"), "off");
    assert!(data.join("skills/bundled/docx/SKILL.md").is_file());
}

#[tokio::test]
async fn disabled_builtins_are_absent_from_the_model_listing_but_stay_listed_with_state() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let data = temp.path().join("data");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new()
        .route("/v1/chat/completions", post(respond))
        .with_state(requests.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_dcl"));
    command
        .current_dir(&workspace)
        .env("CLAUDE_CONFIG_DIR", temp.path().join("empty-user-config"))
        .env("SKILL_FIXTURE_KEY", "fixture-not-secret")
        .args([
            "--data-dir",
            data.to_str().unwrap(),
            "--base-url",
            &endpoint,
            "--api-format",
            "openai",
            "--api-key-env",
            "SKILL_FIXTURE_KEY",
            "--model",
            "fixture",
            "--print",
            "--",
            "Say hello",
        ])
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let output = tokio::time::timeout(Duration::from_secs(60), command.output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let request = requests.lock().unwrap()[0].clone();
    let messages = request["messages"].as_array().unwrap();
    let system = messages
        .iter()
        .find(|message| message["role"] == "system")
        .and_then(|message| message["content"].as_str())
        .expect("system message");
    // The listing travels in a reminder joined to the first user message so the
    // session keeps the skills it started with; it must not return to the
    // system prompt.
    assert!(
        !system.contains("Available skills"),
        "the skill listing must stay out of the system prompt: {system}"
    );
    let first_user = messages
        .iter()
        .find(|message| message["role"] == "user")
        .expect("first user message");
    // Serialize the whole message so this works for both a joined string and
    // per-reminder content parts.
    let user_text = serde_json::to_string(first_user).unwrap();
    let listing = user_text
        .split_once("Available skills")
        .expect("model skill listing in the first user message")
        .1;
    assert!(listing.contains("- skill-creator:"), "{listing}");
    for name in ["task-template", "docx", "pdf", "commit"] {
        assert!(
            !listing.contains(&format!("- {name}:")),
            "{name} must not reach the model listing: {listing}"
        );
    }
    for name in ["task-template", "docx", "pdf", "commit"] {
        assert_eq!(listed_state(&data, name), "off", "{name}");
    }
    server.abort();
}
