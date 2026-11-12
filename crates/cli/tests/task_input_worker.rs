//! A real persistent worker owns an interactive pipe shell across turns.
//!
//! The worker (its engine process) starts the shell; a later turn routed to the
//! same live worker writes stdin and reads the echoed output. Lifecycle follows
//! the documented policy: a shutdown is refused while a shell task is active,
//! `TaskStop` cancels the exact process tree, and only then does a graceful
//! worker shutdown succeed.
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

fn tool_history(body: &Value) -> Vec<(String, Value)> {
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
        .filter_map(|message| {
            let name = names.get(message["tool_call_id"].as_str()?)?;
            let content = message["content"]
                .as_str()
                .and_then(|text| serde_json::from_str::<Value>(text).ok())
                .unwrap_or(Value::Null);
            Some((name.clone(), content))
        })
        .collect()
}

fn last_user_text(body: &Value) -> String {
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|message| message["role"] == "user" && message["content"].is_string())
        .filter_map(|message| message["content"].as_str().map(str::to_owned))
        .next_back()
        .unwrap_or_default()
}

fn raw_tool_message(body: &Value, name: &str) -> Option<String> {
    let messages = body["messages"].as_array()?;
    let mut ids = Vec::new();
    for message in messages {
        for call in message["tool_calls"].as_array().into_iter().flatten() {
            if call["function"]["name"].as_str() == Some(name) {
                if let Some(id) = call["id"].as_str() {
                    ids.push(id.to_owned());
                }
            }
        }
    }
    messages.iter().rev().find_map(|message| {
        let id = message["tool_call_id"].as_str()?;
        if message["role"] == "tool" && ids.iter().any(|candidate| candidate == id) {
            message["content"].as_str().map(str::to_owned)
        } else {
            None
        }
    })
}

fn tool_call_frame(id: &str, name: &str, arguments: &Value) -> Vec<Value> {
    vec![
        json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":name,"arguments":arguments.to_string()}}]},"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
    ]
}

fn text_frames(text: &str) -> Vec<Value> {
    vec![
        json!({"choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
    ]
}

fn stream(frames: Vec<Value>) -> axum::response::Response {
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
    last_input: Arc<Mutex<Option<String>>>,
    requests: Arc<Mutex<Vec<Value>>>,
}

async fn respond(
    State(fixture): State<Fixture>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    fixture.requests.lock().unwrap().push(body.clone());
    let tools = tool_names(&body);
    let history = tool_history(&body);
    // Recalled memories are appended as reminders; only the model's own first
    // line identifies the turn.
    let prompt = last_user_text(&body);
    let head = prompt.lines().next().unwrap_or_default().trim().to_owned();
    let has = |name: &str| history.iter().any(|(call, _)| call == name);
    let has_tool = |name: &str| tools.iter().any(|tool| tool == name);
    let framed = if head == "start the shell" {
        if !has_tool("TaskInput") {
            tool_call_frame(
                "worker-search",
                "ToolSearch",
                &json!({"query":"select:TaskInput"}),
            )
        } else if !has("Bash") {
            let script = fixture
                .script
                .lock()
                .unwrap()
                .clone()
                .expect("the test never provided a shell script");
            tool_call_frame(
                "worker-start",
                "Bash",
                &json!({
                    "command":format!("sh {}", script.display()),
                    "description":"Worker interactive shell",
                    "run_in_background":true,
                    "interaction":"pipe"
                }),
            )
        } else {
            let id = history
                .iter()
                .find(|(name, _)| name == "Bash")
                .and_then(|(_, value)| value["task_id"].as_str().map(str::to_owned))
                .expect("Bash result has no task_id");
            *fixture.task_id.lock().unwrap() = Some(id);
            text_frames("shell started by the worker")
        }
    } else if head == "write to the shell" {
        if let Some(text) = raw_tool_message(&body, "TaskInput") {
            *fixture.last_input.lock().unwrap() = Some(text);
        }
        if !has("Bash") {
            panic!("the second turn lost the first turn's shell: {body}");
        }
        let id = history
            .iter()
            .find(|(name, _)| name == "Bash")
            .and_then(|(_, value)| value["task_id"].as_str().map(str::to_owned))
            .expect("Bash result has no task_id");
        *fixture.task_id.lock().unwrap() = Some(id.clone());
        if !has("TaskInput") {
            tool_call_frame(
                "worker-input",
                "TaskInput",
                &json!({"task_id":id,"data":"hello-worker\n"}),
            )
        } else {
            text_frames("stdin written by the worker")
        }
    } else if head == "stop the shell" {
        let id = history
            .iter()
            .find(|(name, _)| name == "Bash")
            .and_then(|(_, value)| value["task_id"].as_str().map(str::to_owned))
            .expect("Bash result has no task_id");
        if !has("TaskStop") {
            tool_call_frame("worker-stop", "TaskStop", &json!({"task_id":id}))
        } else {
            text_frames("shell stopped by the worker")
        }
    } else {
        panic!("unexpected worker prompt: {prompt}");
    };
    stream(framed)
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

async fn completed(root: &Path, id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let job = deepseek_code_core::services::background_sessions::get(root, id).unwrap();
        if job.finished_ms.is_some() {
            assert_eq!(job.status, "completed", "{job:?}");
            return json!({"status":job.status});
        }
        assert!(Instant::now() < deadline, "worker job {id} never finished");
        tokio::time::sleep(Duration::from_millis(25)).await;
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

/// POSIX `kill(pid, 0)`; a zombie still counts as present.
fn alive(pid: u32) -> bool {
    unsafe extern "C" {
        fn kill(pid: i32, signal: i32) -> i32;
    }
    pid > 0 && pid <= i32::MAX as u32 && unsafe { kill(pid as i32, 0) } == 0
}

/// A process's start time, so a reused PID is distinguishable from the original.
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

#[cfg(not(target_os = "linux"))]
fn start_time(_pid: u32) -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
fn parent_of(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit(')')
        .next()?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// The exact process is gone, not merely its PID reused.
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

fn task_get(root: &Path, session: &str, id: &str) -> Value {
    tasks::execute_for_session(
        "TaskGet",
        json!({"task_id":id,"block":false}),
        root,
        session,
    )
    .unwrap()
}

async fn wait_output(root: &Path, session: &str, id: &str, marker: &str) -> Value {
    let deadline = Instant::now() + READY;
    loop {
        let value = task_get(root, session, id);
        if value["stdout"]
            .as_str()
            .is_some_and(|text| text.contains(marker))
        {
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "no {marker:?} in the shell output within {READY:?}: {value}"
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

#[tokio::test]
async fn worker_owns_pipe_input_across_turns_and_stops_the_exact_tree() {
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
            "echo $$ > '{}'\nsh -c 'echo $$ > \"{}\"; while :; do sleep 1; done' &\nwhile IFS= read -r line; do echo \"echo:$line\"; done\n",
            shell_pid.display(),
            descendant_pid.display()
        ),
    )
    .unwrap();
    *fixture.script.lock().unwrap() = Some(script.clone());

    let executable = std::env::var_os("CLAUDE_SESSION_WORKER_TEST_EXE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_dcl")));
    let host = worker::ensure_worker(&paths.data_dir, &executable, &session, &workspace)
        .await
        .unwrap();
    // The engine process owns the shell; the supervisor owns the engine.
    let engine = host.child_pid.expect("the worker recorded no engine PID");
    let supervisor = host
        .supervisor_pid
        .expect("the worker recorded no supervisor PID");
    let mut cleanup = Cleanup(vec![supervisor, engine]);

    // Turn 1: the model starts the shell. The turn ends while the shell keeps
    // running, so the next turn is routed to the same live worker.
    let start_job = submit(&host, &endpoint, "start the shell").await;
    completed(&paths.data_dir, &start_job).await;
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
    let (shell_started, descendant_started) = (start_time(shell), start_time(descendant));

    // Actually belongs to the worker's tree, not to the test process.
    #[cfg(target_os = "linux")]
    {
        let mut cursor = shell;
        let mut reached = false;
        for _ in 0..6 {
            if cursor == engine {
                reached = true;
                break;
            }
            let Some(parent) = parent_of(cursor) else {
                break;
            };
            cursor = parent;
        }
        assert!(
            reached,
            "the shell {shell} is not under the engine {engine}"
        );
        assert_eq!(
            parent_of(descendant),
            Some(shell),
            "the descendant is not a child of the shell"
        );
    }
    assert!(
        alive(engine) && alive(shell) && alive(descendant),
        "the shell tree did not survive the turn"
    );

    // Turn 2: a later turn on the same live worker writes stdin through the
    // real TaskInput tool; the shell echoes it to its captured stdout.
    let write_job = submit(&host, &endpoint, "write to the shell").await;
    completed(&paths.data_dir, &write_job).await;
    let input_result = fixture
        .last_input
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| "TaskInput was never called".into());
    assert!(
        input_result.contains("bytes_written"),
        "the TaskInput call failed: {input_result}\nprompts={:?}",
        fixture
            .requests
            .lock()
            .unwrap()
            .iter()
            .map(|request| last_user_text(request))
            .collect::<Vec<_>>()
    );
    let echoed = wait_output(&paths.data_dir, &session, &id, "echo:hello-worker").await;
    assert_eq!(echoed["status"], "running", "{echoed}");

    // Documented policy: a worker owning a live shell refuses shutdown.
    let refused = worker::shutdown(&paths.data_dir, &session)
        .await
        .expect_err("the worker accepted shutdown while owning a pipe shell");
    assert!(
        format!("{refused}").contains("background shell tasks"),
        "{refused}"
    );
    assert!(
        alive(engine) && alive(shell) && alive(descendant),
        "a refused shutdown changed the shell tree"
    );

    // Turn 3: explicit TaskStop on the same owner cancels the exact tree.
    let stop_job = submit(&host, &endpoint, "stop the shell").await;
    completed(&paths.data_dir, &stop_job).await;
    wait_gone(descendant, descendant_started, "stopped descendant").await;
    wait_gone(shell, shell_started, "stopped shell").await;
    let stopped = task_get(&paths.data_dir, &session, &id);
    assert_eq!(stopped["status"], "cancelled", "{stopped}");

    // Now shutdown is allowed and the worker's own processes exit.
    let (engine_started, supervisor_started) = (start_time(engine), start_time(supervisor));
    worker::shutdown(&paths.data_dir, &session).await.unwrap();
    wait_gone(engine, engine_started, "stopped engine").await;
    wait_gone(supervisor, supervisor_started, "stopped supervisor").await;
}

#[tokio::test]
async fn a_second_session_cannot_write_the_workers_shell() {
    let root = tempfile::tempdir().unwrap();
    let workspace = root.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let data = root.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let paths = RuntimePaths::new(data.clone()).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let session = store.create(&workspace, "fixture", "cli-runtime").unwrap();
    let other = store.create(&workspace, "fixture", "cli-runtime").unwrap();
    let (endpoint, fixture) = start_server().await;
    let shell_pid = workspace.join("root.pid");
    let descendant_pid = workspace.join("descendant.pid");
    let script = workspace.join("shell.sh");
    std::fs::write(
        &script,
        format!(
            "echo $$ > '{}'\nsh -c 'echo $$ > \"{}\"; while :; do sleep 1; done' &\nwhile IFS= read -r line; do echo \"echo:$line\"; done\n",
            shell_pid.display(),
            descendant_pid.display()
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
    let engine = host.child_pid.unwrap();
    let supervisor = host.supervisor_pid.unwrap();
    let mut cleanup = Cleanup(vec![supervisor, engine]);
    let job = submit(&host, &endpoint, "start the shell").await;
    completed(&paths.data_dir, &job).await;
    let id = fixture.task_id.lock().unwrap().clone().unwrap();
    let shell = wait_for_pid(&shell_pid, "shell").await;
    let descendant = wait_for_pid(&descendant_pid, "descendant").await;
    cleanup.0.push(shell);
    cleanup.0.push(descendant);
    let (shell_started, descendant_started) = (start_time(shell), start_time(descendant));

    // Ownership is the caller's session: another session has no such task.
    let error = tasks::execute_for_session(
        "TaskInput",
        json!({"task_id":id,"data":"foreign"}),
        &paths.data_dir,
        &other,
    )
    .unwrap_err();
    assert!(format!("{error}").contains("Task not found"), "{error}");
    // Nothing reached the shell: no echo, and it is still running.
    let observed = task_get(&paths.data_dir, &session, &id);
    assert_eq!(observed["status"], "running", "{observed}");
    assert_eq!(observed["stdout"], "", "{observed}");

    tasks::execute_for_session("TaskStop", json!({"task_id":id}), &paths.data_dir, &session)
        .unwrap();
    let deadline = Instant::now() + READY;
    while !gone(descendant, descendant_started) || !gone(shell, shell_started) {
        assert!(
            Instant::now() < deadline,
            "the shell tree survived TaskStop"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    worker::shutdown(&paths.data_dir, &session).await.unwrap();
}
