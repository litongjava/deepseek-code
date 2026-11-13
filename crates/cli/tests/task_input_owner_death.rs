//! Abrupt owner death must not leave a writable stdin channel.
//!
//! The shell's owner is the worker's engine process. When that process is killed
//! without cleanup, the task heartbeat stays fresh for up to ten seconds, so a
//! naive `TaskInput` would still see `running`. A dead owner must be refused: a
//! surviving descendant (here one that ignores stdin) is not a healthy input
//! channel.
//!
//! Orphan cleanup after SIGKILL is a separate open item; this test only pins the
//! input contract and removes its exact owned processes.
#![cfg(unix)]
use axum::{extract::State, response::IntoResponse, routing::post, Json, Router};
use deepseek_code_core::{
    runtime::{RuntimePaths, SessionStore},
    services::session_worker as worker,
    tools::tasks,
};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

const READY: Duration = Duration::from_secs(30);

fn tool_names(body: &Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| {
            tool["function"]["name"]
                .as_str()
                .or_else(|| tool["name"].as_str())
                .map(str::to_owned)
        })
        .collect()
}

fn tool_history(body: &Value) -> Vec<String> {
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    let mut names = std::collections::HashMap::new();
    for message in &messages {
        for call in message["tool_calls"].as_array().into_iter().flatten() {
            if let (Some(id), Some(name)) = (call["id"].as_str(), call["function"]["name"].as_str())
            {
                names.insert(id.to_owned(), name.to_owned());
            }
        }
    }
    messages
        .iter()
        .filter(|message| message["role"] == "tool")
        .filter_map(|message| names.get(message["tool_call_id"].as_str()?).cloned())
        .collect()
}

/// The task id from the Bash tool result, not from its arguments.
fn bash_task_id(body: &Value) -> Option<String> {
    let messages = body["messages"].as_array()?;
    let call = messages.iter().find_map(|message| {
        message["tool_calls"]
            .as_array()?
            .iter()
            .find(|call| call["function"]["name"] == "Bash")
            .and_then(|call| call["id"].as_str())
    })?;
    messages.iter().find_map(|message| {
        if message["role"] == "tool" && message["tool_call_id"].as_str() == Some(call) {
            message["content"]
                .as_str()
                .and_then(|text| serde_json::from_str::<Value>(text).ok())
                .and_then(|value| value["task_id"].as_str().map(str::to_owned))
        } else {
            None
        }
    })
}

fn frames(frames: Vec<Value>) -> axum::response::Response {
    let mut body: String = frames
        .into_iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect();
    body.push_str("data: [DONE]\n\n");
    ([("content-type", "text/event-stream")], body).into_response()
}

#[derive(Clone, Default)]
struct Fixture {
    script: Arc<Mutex<Option<PathBuf>>>,
    task_id: Arc<Mutex<Option<String>>>,
}

async fn respond(
    State(fixture): State<Fixture>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    let tools = tool_names(&body);
    let history = tool_history(&body);
    let has = |name: &str| history.iter().any(|call| call == name);
    let framed = if !tools.iter().any(|tool| tool == "TaskInput") {
        vec![
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"death-search","type":"function","function":{"name":"ToolSearch","arguments":json!({"query":"select:TaskInput"}).to_string()}}]},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
        ]
    } else if !has("Bash") {
        let script = fixture.script.lock().unwrap().clone().unwrap();
        vec![
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"death-start","type":"function","function":{"name":"Bash","arguments":json!({"command":format!("sh {}", script.display()),"description":"Abrupt-death shell","run_in_background":true,"interaction":"pipe"}).to_string()}}]},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
        ]
    } else {
        let id = bash_task_id(&body);
        if let Some(id) = id {
            *fixture.task_id.lock().unwrap() = Some(id);
        }
        vec![
            json!({"choices":[{"index":0,"delta":{"content":"shell started"},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
        ]
    };
    frames(framed)
}

async fn start_server() -> (String, Fixture) {
    let fixture = Fixture::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/chat/completions", post(respond))
        .with_state(fixture.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, fixture)
}

async fn submit(host: &worker::Worker, endpoint: &str, prompt: &str) -> String {
    let id = uuid::Uuid::new_v4().to_string();
    let arguments = vec![
        "--data-dir".into(),
        host.root.to_string_lossy().into_owned(),
        "--resume".into(),
        host.session_id.clone(),
        "--base-url".into(),
        format!("{endpoint}/v1"),
        "--api-format".into(),
        "openai".into(),
        "--api-key-env".into(),
        "DEEPSEEK_CODE_CHILD_API_KEY".into(),
        "--model".into(),
        "fixture".into(),
        "--max-tokens".into(),
        "4096".into(),
        "--permission-mode".into(),
        "bypass".into(),
        "--print".into(),
        "--output-format".into(),
        "stream-json".into(),
        "--".into(),
        prompt.into(),
    ];
    let environment = vec![(
        "DEEPSEEK_CODE_CHILD_API_KEY".into(),
        "fixture-secret-only-in-memory".into(),
    )];
    worker::call(
        host,
        &worker::Request::Submit {
            id: id.clone(),
            prompt: prompt.into(),
            arguments,
            environment,
        },
    )
    .await
    .unwrap();
    id
}

async fn completed(root: &Path, id: &str) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let job = deepseek_code_core::services::background_sessions::get(root, id).unwrap();
        if job.finished_ms.is_some() {
            assert_eq!(job.status, "completed", "{job:?}");
            return;
        }
        assert!(Instant::now() < deadline, "worker job {id} never finished");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn alive(pid: u32) -> bool {
    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }
    pid > 0 && pid <= i32::MAX as u32 && unsafe { kill(pid as i32, 0) } == 0
}

#[cfg(target_os = "linux")]
fn start_time(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit(')')
        .next()?
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

/// Other Unix targets have no `/proc`; liveness still works, start identity is
/// unavailable, so a reused PID cannot be distinguished there.
#[cfg(not(target_os = "linux"))]
fn start_time(_pid: u32) -> Option<u64> {
    None
}

fn gone(pid: u32, started: Option<u64>) -> bool {
    if !alive(pid) {
        return true;
    }
    #[cfg(target_os = "linux")]
    {
        started.is_some() && start_time(pid) != started
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = started;
        false
    }
}

async fn wait_gone(pid: u32, started: Option<u64>, what: &str) {
    let deadline = Instant::now() + READY;
    while !gone(pid, started) {
        assert!(
            Instant::now() < deadline,
            "{what} {pid} was still running after {READY:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn read_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

async fn wait_for_pid(path: &Path, what: &str) -> u32 {
    let deadline = Instant::now() + READY;
    loop {
        if let Some(pid) = read_pid(path) {
            return pid;
        }
        assert!(
            Instant::now() < deadline,
            "no {what} PID at {} within {READY:?}",
            path.display()
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

struct Cleanup(Vec<u32>);
impl Drop for Cleanup {
    fn drop(&mut self) {
        for pid in &self.0 {
            unsafe extern "C" {
                fn kill(pid: i32, signal: i32) -> i32;
            }
            if *pid > 0 && *pid <= i32::MAX as u32 {
                unsafe {
                    kill(*pid as i32, 9);
                }
            }
        }
    }
}

fn kill_hard(pid: u32) {
    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }
    if pid > 0 && pid <= i32::MAX as u32 {
        unsafe {
            kill(pid as i32, 9);
        }
    }
}

#[tokio::test]
async fn abrupt_owner_death_does_not_leave_a_writable_stdin() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let data = root.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let paths = RuntimePaths::new(data.clone()).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let session = store.create(&workspace, "fixture", "cli-runtime").unwrap();
    let (endpoint, fixture) = start_server().await;
    let shell_pid = workspace.join("root.pid");
    let descendant_pid = workspace.join("descendant.pid");
    let script = workspace.join("shell.sh");
    std::fs::write(
        &script,
        format!(
            concat!(
                "echo $$ > '{root}'\n",
                // The shell duplicates its stdin before backgrounding, because a
                // non-interactive shell redirects a background list's stdin to
                // /dev/null. fd 3 survives, so the descendant keeps the pipe's
                // read end open even though it never reads data.
                "exec 3<&0\n",
                "( exec 0<&3; while :; do sleep 1; done ) &\n",
                // `$!` is the subshell's real PID; inside a subshell `$$` is
                // still the parent shell's PID.
                "echo $! > '{descendant}'\n",
                "while IFS= read -r line; do echo \"echo:$line\"; done\n"
            ),
            root = shell_pid.display(),
            descendant = descendant_pid.display()
        ),
    )
    .unwrap();
    *fixture.script.lock().unwrap() = Some(script);
    let executable = std::env::var_os("CLAUDE_SESSION_WORKER_TEST_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_dcl")));
    let host = worker::ensure_worker(&paths.data_dir, &executable, &session, &workspace)
        .await
        .unwrap();
    let engine = host.child_pid.expect("no engine PID");
    let supervisor = host.supervisor_pid.expect("no supervisor PID");
    let mut cleanup = Cleanup(vec![supervisor, engine]);
    let job = submit(&host, &endpoint, "start the shell").await;
    completed(&paths.data_dir, &job).await;
    let id = fixture
        .task_id
        .lock()
        .unwrap()
        .clone()
        .expect("the Bash result never carried a task id");
    let shell = wait_for_pid(&shell_pid, "shell").await;
    let descendant = wait_for_pid(&descendant_pid, "descendant").await;
    cleanup.0.push(shell);
    cleanup.0.push(descendant);
    let (engine_started, shell_started, descendant_started) = (
        start_time(engine),
        start_time(shell),
        start_time(descendant),
    );

    // The worker engine is the shell owner. Kill it abruptly: no destructors,
    // no cancel, no graceful shutdown.
    kill_hard(engine);
    kill_hard(supervisor);
    wait_gone(engine, engine_started, "killed engine").await;
    wait_gone(supervisor, start_time(supervisor), "killed supervisor").await;
    // The heartbeat file is still fresh: the old logic would report `running`.
    let directory = paths.data_dir.join("shell-tasks").join(&session).join(&id);
    let age = std::fs::metadata(directory.join("heartbeat"))
        .unwrap()
        .modified()
        .unwrap()
        .elapsed()
        .unwrap();
    assert!(
        age < Duration::from_secs(10),
        "the fixture no longer has a fresh heartbeat: {age:?}"
    );
    // The recorded owner is dead, so the presented status must say so.
    let status = tasks::execute_for_session(
        "TaskGet",
        json!({"task_id":id,"block":false}),
        &paths.data_dir,
        &session,
    )
    .unwrap();
    assert_eq!(status["status"], "unresponsive", "{status}");

    assert_ne!(shell, descendant, "the descendant PID was the shell's own");
    assert!(alive(descendant), "the descendant was not running");
    // The descendant still holds the FIFO read end, so a write would otherwise
    // be accepted by the operating system. Opening the write side succeeds only
    // when a reader exists; the bound keeps a missing reader from hanging.
    let fifo = directory.join("stdin.pipe");
    #[cfg(target_os = "linux")]
    {
        let mut exact = false;
        if let Ok(entries) = std::fs::read_dir(format!("/proc/{descendant}/fd")) {
            for entry in entries.flatten() {
                if std::fs::read_link(entry.path()).is_ok_and(|target| target == fifo) {
                    exact = true;
                }
            }
        }
        assert!(
            exact,
            "the recorded descendant {descendant} does not hold the FIFO read end"
        );
    }
    let mut probe = tokio::process::Command::new("sh")
        .arg("-c")
        .arg(format!("exec 3>{}", fifo.display()))
        .kill_on_drop(true)
        .spawn()
        .expect("starting the FIFO reader probe");
    let opened = tokio::time::timeout(Duration::from_secs(2), probe.wait()).await;
    assert!(
        opened.is_ok(),
        "no reader held the FIFO, so this run would not prove the owner check"
    );
    assert!(opened.unwrap().unwrap().success());

    // Desired contract: a dead owner is not an available input channel.
    let write = tasks::execute_for_session(
        "TaskInput",
        json!({"task_id":id,"data":"ghost\n"}),
        &paths.data_dir,
        &session,
    );
    let error = write.expect_err("a dead owner still accepted stdin");
    assert!(
        format!("{error}").contains("owner lease is released"),
        "the refusal came from something other than the owner lease check: {error:#}"
    );
    // No bytes reached the orphaned tree.
    let echoed = tasks::execute_for_session(
        "TaskGet",
        json!({"task_id":id,"block":false}),
        &paths.data_dir,
        &session,
    )
    .unwrap();
    assert!(
        !echoed["stdout"]
            .as_str()
            .is_some_and(|text| text.contains("ghost")),
        "{echoed}"
    );

    // Cleanup only the exact recorded processes.
    kill_hard(descendant);
    kill_hard(shell);
    wait_gone(descendant, descendant_started, "orphaned descendant").await;
    wait_gone(shell, shell_started, "orphaned shell").await;
}

/// Engine-only death is a different case from killing both owner processes: the
/// worker supervisor is still running and may clean up. This test asserts the
/// input contract (a dead owner is not writable) and records the tree outcome
/// instead of encoding either behavior as desired.
#[tokio::test]
async fn engine_only_death_refuses_input_and_records_the_tree_outcome() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let data = root.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let paths = RuntimePaths::new(data.clone()).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let session = store.create(&workspace, "fixture", "cli-runtime").unwrap();
    let (endpoint, fixture) = start_server().await;
    let shell_pid = workspace.join("root.pid");
    let descendant_pid = workspace.join("descendant.pid");
    let script = workspace.join("shell.sh");
    std::fs::write(
        &script,
        format!(
            concat!(
                "echo $$ > '{root}'\n",
                "exec 3<&0\n",
                "( exec 0<&3; while :; do sleep 1; done ) &\n",
                "echo $! > '{descendant}'\n",
                "while IFS= read -r line; do echo \"echo:$line\"; done\n"
            ),
            root = shell_pid.display(),
            descendant = descendant_pid.display()
        ),
    )
    .unwrap();
    *fixture.script.lock().unwrap() = Some(script);
    let executable = std::env::var_os("CLAUDE_SESSION_WORKER_TEST_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_dcl")));
    let host = worker::ensure_worker(&paths.data_dir, &executable, &session, &workspace)
        .await
        .unwrap();
    let engine = host.child_pid.expect("no engine PID");
    let supervisor = host.supervisor_pid.expect("no supervisor PID");
    let mut cleanup = Cleanup(vec![supervisor, engine]);
    let job = submit(&host, &endpoint, "start the shell").await;
    completed(&paths.data_dir, &job).await;
    let id = fixture
        .task_id
        .lock()
        .unwrap()
        .clone()
        .expect("the Bash result never carried a task id");
    let shell = wait_for_pid(&shell_pid, "shell").await;
    let descendant = wait_for_pid(&descendant_pid, "descendant").await;
    cleanup.0.push(shell);
    cleanup.0.push(descendant);
    let (engine_started, shell_started, descendant_started) = (
        start_time(engine),
        start_time(shell),
        start_time(descendant),
    );

    // Only the engine (the shell's owner) is killed; the supervisor stays.
    kill_hard(engine);
    wait_gone(engine, engine_started, "killed engine").await;

    // Input contract: the kernel released the owner lease with the process, so
    // the channel is refused even though the descendant still holds the FIFO.
    let status = tasks::execute_for_session(
        "TaskGet",
        json!({"task_id":id,"block":false}),
        &paths.data_dir,
        &session,
    )
    .unwrap();
    assert_eq!(status["status"], "unresponsive", "{status}");
    let error = tasks::execute_for_session(
        "TaskInput",
        json!({"task_id":id,"data":"ghost\n"}),
        &paths.data_dir,
        &session,
    )
    .expect_err("an engine-only death still accepted stdin");
    assert!(
        format!("{error}").contains("owner lease is released"),
        "{error:#}"
    );
    let echoed = tasks::execute_for_session(
        "TaskGet",
        json!({"task_id":id,"block":false}),
        &paths.data_dir,
        &session,
    )
    .unwrap();
    assert!(
        !echoed["stdout"]
            .as_str()
            .is_some_and(|text| text.contains("ghost")),
        "{echoed}"
    );

    // Measurement, not a desired outcome: how long the surviving supervisor
    // keeps running and whether the shell tree survives.
    let observed_at_start = (alive(shell), alive(descendant), alive(supervisor));
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    while alive(supervisor) && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    eprintln!(
        "engine-only death: shell_and_descendant_and_supervisor_at_death={observed_at_start:?} \
         supervisor_exited={} shell_alive={} descendant_alive={}",
        !alive(supervisor),
        alive(shell),
        alive(descendant)
    );

    // Cleanup only the exact recorded processes.
    kill_hard(descendant);
    kill_hard(shell);
    kill_hard(supervisor);
    wait_gone(descendant, descendant_started, "orphaned descendant").await;
    wait_gone(shell, shell_started, "orphaned shell").await;
}
