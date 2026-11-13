//! A real `dcl` process must cancel its own active work on SIGTERM/SIGHUP and
//! only then exit nonzero. The model fixture asks for a foreground Bash tool
//! or the Codex external-agent tool; both start a `sleep 30 & wait` subtree in
//! their own process group. Only the processes this test observes are checked
//! or stopped, never every process with a matching name.
#![cfg(unix)]

use axum::{
    extract::State,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde_json::{json, Value};
use std::{
    os::unix::process::ExitStatusExt,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::process::{Child, Command};

#[derive(Clone)]
struct Fixture {
    requests: Arc<Mutex<Vec<Value>>>,
    tool: &'static str,
    arguments: Value,
}

async fn respond(State(state): State<Fixture>, Json(body): Json<Value>) -> Response {
    // Prompt-suggestion and other non-streaming calls only need a title.
    if body["stream"] != true {
        return Json(json!({
            "id": "summary", "object": "chat.completion", "created": 0, "model": "fixture-model",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "title"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 8, "completion_tokens": 3, "total_tokens": 11}
        }))
        .into_response();
    }
    state.requests.lock().unwrap().push(body.clone());
    let last = body["messages"]
        .as_array()
        .and_then(|messages| messages.last());
    let delta = if last.is_some_and(|message| message["role"] != "tool") {
        json!({"tool_calls": [{"index": 0, "id": "call-fixture", "type": "function",
            "function": {"name": state.tool, "arguments": state.arguments.to_string()}}]})
    } else {
        json!({"content": "fixture finished"})
    };
    let finish = if last.is_some_and(|message| message["role"] != "tool") {
        "tool_calls"
    } else {
        "stop"
    };
    let chunks = [
        json!({"id": "fixture", "model": "fixture-model", "choices": [{"index": 0, "delta": delta, "finish_reason": null}]}),
        json!({"id": "fixture", "model": "fixture-model",
            "choices": [{"index": 0, "delta": {}, "finish_reason": finish}],
            "usage": {"prompt_tokens": 12, "completion_tokens": 8}}),
    ];
    let body = chunks
        .iter()
        .map(|chunk| format!("data: {chunk}\n\n"))
        .collect::<String>()
        + "data: [DONE]\n\n";
    ([("content-type", "text/event-stream")], body).into_response()
}

async fn server(tool: &'static str, arguments: Value) -> (String, tokio::task::JoinHandle<()>) {
    let fixture = Fixture {
        requests: Arc::new(Mutex::new(Vec::new())),
        tool,
        arguments,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/chat/completions", post(respond))
        .with_state(fixture);
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (url, task)
}

/// A project and data directory with nothing inherited from the developer's
/// own dcl configuration.
fn workspace(temp: &Path) -> (PathBuf, PathBuf) {
    let data = temp.join("data");
    let project = temp.join("project");
    std::fs::create_dir_all(data.join("config")).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    (data, project)
}

fn command(data: &Path, project: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dcl"));
    command
        .args([
            "--data-dir",
            data.to_str().unwrap(),
            "--path",
            project.to_str().unwrap(),
            "--model",
            "fixture-model",
            "--api-format",
            "openai",
            "--api-key-env",
            "INTERRUPT_FIXTURE_KEY",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for key in [
        "DEEPSEEK_CODE_DATA_DIR",
        "DEEPSEEK_CODE_BACKGROUND_CONTROL",
        "DEEPSEEK_CODE_BACKGROUND_JOB",
        "DEEPSEEK_CODE_WORKER_MODEL",
        "DEEPSEEK_CODE_WORKER_AUTO_ROUTE",
        "DEEPSEEK_CODE_CHILD_API_KEY",
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "DEEPSEEK_API_KEY",
    ] {
        command.env_remove(key);
    }
    command.env("INTERRUPT_FIXTURE_KEY", "fixture-only-secret");
    command
}

fn alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

fn pid_from(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.trim().parse().ok())
}

async fn wait_for_pid(path: &Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Some(pid) = pid_from(path).filter(|pid| alive(*pid)) {
            return pid;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the fixture subtree never started: {}", path.display());
}

fn signal(pid: u32, name: &str) {
    let status = std::process::Command::new("kill")
        .args([name, &pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success(), "kill {name} {pid}");
}

/// Waits for the child and reports whether it ended by a signal.
async fn wait_for_exit(child: &mut Child) -> (ExitStatus, bool) {
    match tokio::time::timeout(Duration::from_secs(20), child.wait()).await {
        Ok(status) => {
            let status = status.unwrap();
            (status, status.signal().is_some())
        }
        Err(_) => {
            let _ = child.start_kill();
            let status = child.wait().await.unwrap();
            (status, false)
        }
    }
}

/// Polls until every observed process is gone; returns the ones still alive.
async fn survivors(pids: &[u32]) -> Vec<u32> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left: Vec<u32> = pids.iter().copied().filter(|pid| alive(*pid)).collect();
        if left.is_empty() || Instant::now() >= deadline {
            return left;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Best-effort cleanup so a failing assertion cannot leave the test subtree
/// running. Only the pids this test observed are signalled.
fn reap(pids: &[u32]) {
    for pid in pids {
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status();
    }
}

fn sleep_tree_script(marker: &Path, parent_marker: Option<&Path>) -> String {
    let parent = parent_marker
        .map(|path| format!("echo $$ > '{}'\n", path.display()))
        .unwrap_or_default();
    format!(
        "#!/bin/sh\n{parent}sh -c 'sleep 30 & echo $! > \"{}\"; wait' &\nwait\n",
        marker.display()
    )
}

#[cfg(unix)]
fn write_executable(path: &Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, contents).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

async fn interrupt_foreground_bash(signal_name: &str) {
    let temp = tempfile::tempdir().unwrap();
    let (data, project) = workspace(temp.path());
    let marker = temp.path().join("bash-child.pid");
    let arguments = json!({
        "command": format!("sh -c 'sleep 30 & echo $! > \"{}\"; wait'", marker.display()),
        "description": "Hold a foreground shell open"
    });
    let (url, server) = server("Bash", arguments).await;
    let mut child = command(&data, &project)
        .args([
            "--base-url",
            &url,
            "--permission-mode",
            "bypass",
            "query",
            "-p",
            "hold the shell open",
        ])
        .spawn()
        .unwrap();
    let dcl = child.id().unwrap();
    let sleep_pid = wait_for_pid(&marker).await;
    assert!(alive(dcl), "dcl exited before the signal");
    signal(dcl, signal_name);
    let (status, by_signal) = wait_for_exit(&mut child).await;
    let left = survivors(&[sleep_pid]).await;
    reap(&[sleep_pid, dcl]);
    server.abort();
    assert!(
        !status.success(),
        "{signal_name}: the interrupted turn must not report success"
    );
    assert!(
        !by_signal,
        "{signal_name}: dcl must cancel the turn and exit on its own, not die from the signal (survivors after the wait: {left:?})"
    );
    assert!(
        left.is_empty(),
        "{signal_name}: the foreground Bash subtree survived: {left:?}"
    );
}

#[tokio::test]
async fn sigterm_cancels_a_foreground_bash_subtree_then_exits_nonzero() {
    interrupt_foreground_bash("-TERM").await;
}

#[tokio::test]
async fn sighup_cancels_a_foreground_bash_subtree_then_exits_nonzero() {
    interrupt_foreground_bash("-HUP").await;
}

#[tokio::test]
async fn sigterm_cancels_the_codex_agent_tree_then_exits_nonzero() {
    let temp = tempfile::tempdir().unwrap();
    let (data, project) = workspace(temp.path());
    // Offer the Codex tool: enabled and resident, with a controlled executable
    // first on PATH. The script never emits lifecycle events; it only starts a
    // process tree, so the measurement is the cancellation, not the protocol.
    std::fs::write(
        data.join("customization.json"),
        json!({"tool_loading": {"Codex": {"enabled": true, "on_demand": false}}}).to_string(),
    )
    .unwrap();
    let bin = temp.path().join("fake-bin");
    std::fs::create_dir(&bin).unwrap();
    let sleep_marker = temp.path().join("codex-child.pid");
    let codex_marker = temp.path().join("codex-parent.pid");
    write_executable(
        &bin.join("codex"),
        &sleep_tree_script(&sleep_marker, Some(&codex_marker)),
    );
    let (url, server) = server("Codex", json!({"prompt": "hold the agent open"})).await;
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut child = command(&data, &project)
        .env("PATH", path)
        .args([
            "--base-url",
            &url,
            "--permission-mode",
            "bypass",
            "query",
            "-p",
            "delegate the work",
        ])
        .spawn()
        .unwrap();
    let dcl = child.id().unwrap();
    let sleep_pid = wait_for_pid(&sleep_marker).await;
    let codex_pid = wait_for_pid(&codex_marker).await;
    assert!(alive(dcl), "dcl exited before the signal");
    signal(dcl, "-TERM");
    let (status, by_signal) = wait_for_exit(&mut child).await;
    let left = survivors(&[sleep_pid, codex_pid]).await;
    reap(&[sleep_pid, codex_pid, dcl]);
    server.abort();
    assert!(
        !status.success(),
        "the interrupted turn must not report success"
    );
    assert!(
        !by_signal,
        "dcl must cancel the turn and exit on its own, not die from the signal (survivors after the wait: {left:?})"
    );
    assert!(left.is_empty(), "the Codex tool subtree survived: {left:?}");
}
