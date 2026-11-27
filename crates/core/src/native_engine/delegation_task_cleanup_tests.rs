//! A delegated child runs in its own conversation. Its managed shells and
//! monitors are stopped when it ends and reported in the parent's result.
use super::*;
use crate::native_engine::{engine_core::ReasoningMode, provider_manager::ResolvedProvider};
use crate::runtime::{RuntimePaths, SessionStore};
use axum::{extract::State, routing::post, Json, Router};
use serde_json::Value;
use std::{
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

#[derive(Clone)]
struct MockModel {
    index: Arc<AtomicUsize>,
    /// Arguments for the background tool call.
    background: Value,
    /// Answer the next request with an authentication error instead of
    /// finishing the child normally.
    fail_after_background: bool,
    /// Gates a Monitor-using child's final response until its processes are pinned.
    monitor_finish: Option<Arc<tokio::sync::Notify>>,
}

fn provider(endpoint: &str) -> ResolvedProvider {
    ResolvedProvider {
        provider: serde_json::from_value(json!({"id":"fixture","name":"fixture","base_url":endpoint,"api_key":"unused","api_format":"openai","models":[],"enabled":true})).unwrap(),
        model: serde_json::from_value(json!({"id":"fixture","name":"fixture","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap(),
    }
}

fn tool_call_frame(name: &str, id: &str, arguments: Value) -> String {
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":name,"arguments":arguments.to_string()}}]},"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]})
    )
}

fn text_frame(text: &str) -> String {
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})
    )
}

async fn reply(
    State(mock): State<MockModel>,
    Json(_body): Json<Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let index = mock.index.fetch_add(1, Ordering::SeqCst);
    if let Some(finish) = mock.monitor_finish {
        let body = match index {
            0 => tool_call_frame(
                "ToolSearch",
                "monitor-search",
                json!({"query":"select:Monitor"}),
            ),
            1 => tool_call_frame("Monitor", "monitor-call", mock.background),
            _ => {
                finish.notified().await;
                text_frame("child complete")
            }
        };
        return ([("content-type", "text/event-stream")], body).into_response();
    }
    if mock.fail_after_background && index > 0 {
        // Give the background shell time to publish its PID before the request
        // fails and the cleanup stops it. Authentication failures are not
        // retried, so the child fails immediately.
        tokio::time::sleep(Duration::from_millis(300)).await;
        return (
            axum::http::StatusCode::UNAUTHORIZED,
            Json(json!({"error":{"message":"fixture provider rejected the request"}})),
        )
            .into_response();
    }
    let body = match index {
        // The background call the test examines.
        0 => tool_call_frame("Bash", "background-call", mock.background.clone()),
        // A foreground call long enough for the background task to settle.
        1 => tool_call_frame("Bash", "settle-call", json!({"command":"sleep 1"})),
        _ => text_frame("child complete"),
    };
    ([("content-type", "text/event-stream")], body).into_response()
}

struct Fixture {
    temp: tempfile::TempDir,
    data: std::path::PathBuf,
    parent: String,
    endpoint: String,
    server: tokio::task::JoinHandle<()>,
}

async fn fixture(background: Value, fail_after_background: bool) -> Fixture {
    fixture_with_model(MockModel {
        index: Arc::new(AtomicUsize::new(0)),
        background,
        fail_after_background,
        monitor_finish: None,
    })
    .await
}

async fn fixture_with_model(mock: MockModel) -> Fixture {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
    let sessions = SessionStore::new(paths.open_database().unwrap());
    let parent = sessions.create(temp.path(), "fixture", "fixture").unwrap();
    let app = Router::new()
        .route("/v1/chat/completions", post(reply))
        .with_state(mock);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Fixture {
        data: paths.data_dir.clone(),
        temp,
        parent,
        endpoint,
        server,
    }
}

impl Fixture {
    fn executor(&self) -> ToolLoopExecutor {
        let (events, _receiver) = tokio::sync::mpsc::channel(128);
        let manager = crate::runtime::default_permissions();
        manager.set_mode(crate::permissions::PermissionMode::BypassPermissions);
        ToolLoopExecutor::new(
            provider(&self.endpoint),
            vec![],
            None,
            128,
            events,
            self.temp.path().to_string_lossy().into_owned(),
            ReasoningMode::Standard,
        )
        .with_conv_id(self.parent.clone())
        .with_search_config_path(
            crate::data_layout::DataLayout::from_root(&self.data).config_file("settings.json"),
        )
        .with_permission_manager(manager)
    }

    async fn delegate(&self) -> Value {
        tokio::time::timeout(
            Duration::from_secs(30),
            self.executor()
                .execute_delegated(&json!({"prompt":"run a background command","agent_type":"general"})),
        )
        .await
        .unwrap()
        .unwrap()
    }
}

fn managed_task_if_any(root: &Path, session: &str) -> Option<(String, Value)> {
    let listing =
        crate::tools::tasks::execute_for_session("TaskList", json!({}), root, session).ok()?;
    listing["tasks"]
        .as_array()?
        .iter()
        .find(|task| task["metadata"]["managed_shell"] == true)
        .map(|task| (task["id"].as_str().unwrap().to_owned(), task.clone()))
}

fn managed_task(root: &Path, session: &str) -> (String, Value) {
    managed_task_if_any(root, session)
        .unwrap_or_else(|| panic!("no managed shell task in session {session}"))
}

fn read_status(root: &Path, session: &str, id: &str) -> String {
    let value = crate::tools::tasks::execute_for_session(
        "TaskGet",
        json!({"task_id":id,"block":false}),
        root,
        session,
    )
    .unwrap();
    value["status"].as_str().unwrap_or("unknown").to_owned()
}

async fn wait_for_status(root: &Path, session: &str, id: &str, expected: &str) -> Value {
    let started = std::time::Instant::now();
    loop {
        let value = crate::tools::tasks::execute_for_session(
            "TaskGet",
            json!({"task_id":id,"block":false}),
            root,
            session,
        )
        .unwrap();
        let status = value["status"].as_str().unwrap_or("unknown");
        if status == expected {
            return value;
        }
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "task {id} stayed {status} instead of {expected}: {value}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The conversation that owns the only task database the child created.
async fn wait_for_child_session(root: &Path, parent: &str) -> String {
    let started = std::time::Instant::now();
    loop {
        if let Ok(entries) = std::fs::read_dir(root.join("data/session-tasks")) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|value| value.to_str()) != Some("sqlite3") {
                    continue;
                }
                let Some(session) = path.file_stem().and_then(|value| value.to_str()) else {
                    continue;
                };
                if session != parent {
                    return session.to_owned();
                }
            }
        }
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "the child published no task database"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_managed_task(root: &Path, session: &str) -> (String, Value) {
    let started = std::time::Instant::now();
    loop {
        if let Some((id, task)) = managed_task_if_any(root, session) {
            if task["status"] == "running" {
                return (id, task);
            }
        }
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "the child published no running managed shell"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_pid(path: &Path) -> u32 {
    let started = std::time::Instant::now();
    loop {
        if let Some(pid) = read_pid(path) {
            return pid;
        }
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "the background shell recorded no PID"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_process_gone(pid: u32, what: &str) {
    let started = std::time::Instant::now();
    while !process_finished(pid) {
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "{what} {pid} is still running"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn read_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn process_finished(pid: u32) -> bool {
    crate::services::session_worker::process_exited(pid)
}

#[cfg(unix)]
#[tokio::test]
async fn a_subagent_background_task_is_stopped_when_the_subagent_ends() {
    let temp = tempfile::tempdir().unwrap();
    let pid_file = temp.path().join("subagent-shell.pid");
    let command = format!("echo $$ > {}; exec sleep 30", pid_file.display());
    let fixture = fixture(json!({
        "command": command,
        "run_in_background": true,
        "description": "leave running"
    }), false)
    .await;

    let result = fixture.delegate().await;
    assert_eq!(result["status"], "completed", "{result}");
    let child = result["session_id"].as_str().unwrap();
    let text = result["result"].as_str().unwrap();
    assert!(
        text.contains(
            "Stopped 1 background task(s) the subagent left running: leave running."
        ),
        "{result}"
    );

    let (id, task) = managed_task(&fixture.data, child);
    assert_eq!(task["subject"], "leave running");
    let cancelled = wait_for_status(&fixture.data, child, &id, "cancelled").await;
    assert_eq!(cancelled["status"], "cancelled", "{cancelled}");

    let pid = read_pid(&pid_file).expect("the background shell recorded its PID");
    wait_process_gone(pid, "the stopped background process").await;
    fixture.server.abort();
}

#[tokio::test]
async fn a_finished_subagent_task_is_left_alone() {
    let fixture = fixture(json!({
        "command": "echo quick",
        "run_in_background": true,
        "description": "quick task"
    }), false)
    .await;

    let result = fixture.delegate().await;
    assert_eq!(result["status"], "completed", "{result}");
    assert_eq!(result["result"], "child complete", "{result}");
    let child = result["session_id"].as_str().unwrap();

    let (id, _) = managed_task(&fixture.data, child);
    let completed = wait_for_status(&fixture.data, child, &id, "completed").await;
    assert_eq!(completed["status"], "completed", "{completed}");
    fixture.server.abort();
}

/// The cleanup summary must reach the parent on the error path as well.
#[cfg(unix)]
#[tokio::test]
async fn a_failed_subagent_still_reports_the_stopped_tasks() {
    let temp = tempfile::tempdir().unwrap();
    let pid_file = temp.path().join("failed-subagent-shell.pid");
    let command = format!("echo $$ > {}; exec sleep 30", pid_file.display());
    let fixture = fixture(
        json!({
            "command": command,
            "run_in_background": true,
            "description": "left by a failing child"
        }),
        true,
    )
    .await;

    let error = tokio::time::timeout(
        Duration::from_secs(30),
        fixture
            .executor()
            .execute_delegated(&json!({"prompt":"run and fail","agent_type":"general"})),
    )
    .await
    .unwrap()
    .unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains(
            "Stopped 1 background task(s) the subagent left running: left by a failing child."
        ),
        "{message}"
    );

    let child = wait_for_child_session(&fixture.data, &fixture.parent).await;
    let (id, _) = managed_task(&fixture.data, &child);
    // The stop was confirmed before the error reached the parent.
    assert_eq!(read_status(&fixture.data, &child, &id), "cancelled");
    let pid = wait_for_pid(&pid_file).await;
    wait_process_gone(pid, "the failing subagent's shell").await;
    fixture.server.abort();
}

/// A session-wide scan failure must reach the parent as a warning line instead
/// of looking like a checked session with nothing to stop.
#[tokio::test]
async fn a_scan_error_is_reported_to_the_parent() {
    let root = tempfile::tempdir().unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    // The task store opens this path: a directory there cannot be a database.
    std::fs::create_dir_all(
        root.path()
            .join("data/session-tasks")
            .join(format!("{session}.sqlite3")),
    )
    .unwrap();
    let mut cleanup = ChildSessionTasks::new(Some(root.path().to_path_buf()), session);
    let stop = cleanup.stop().await;
    let summary = stop_summary(&stop);
    assert!(
        summary.contains("Could not check the subagent's background tasks:"),
        "{summary}"
    );
    assert!(stop.stopped.is_empty(), "{stop:?}");
    assert!(stop.requested.is_empty(), "{stop:?}");
}

/// Dropping the owner while the child still runs must not leave its shell
/// behind: the guard inside the child's own task requests the stop when the
/// child future is dropped, and the shell owner confirms it.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn aborting_the_owner_stops_a_task_created_late() {
    let temp = tempfile::tempdir().unwrap();
    let pid_file = temp.path().join("aborted-subagent-shell.pid");
    let command = format!("echo $$ > {}; exec sleep 30", pid_file.display());
    let fixture = fixture(
        json!({
            "command": command,
            "run_in_background": true,
            "description": "left by an aborted child"
        }),
        false,
    )
    .await;
    let executor = fixture.executor();
    let owner = tokio::spawn(async move {
        executor
            .execute_delegated(&json!({"prompt":"run a background command","agent_type":"general"}))
            .await
    });

    let child = wait_for_child_session(&fixture.data, &fixture.parent).await;
    let (id, _) = wait_for_managed_task(&fixture.data, &child).await;
    let pid = wait_for_pid(&pid_file).await;
    owner.abort();
    assert!(owner.await.unwrap_err().is_cancelled());

    let cancelled = wait_for_status(&fixture.data, &child, &id, "cancelled").await;
    assert_eq!(cancelled["status"], "cancelled", "{cancelled}");
    wait_process_gone(pid, "the aborted subagent's shell").await;
    fixture.server.abort();
}

#[cfg(target_os = "linux")]
#[derive(PartialEq, Eq)]
enum MonitorOwnerExit {
    Return,
    Abort,
    ReturnAfterTaskUpdate,
    ReturnWithHistoricalStatus,
}

#[cfg(target_os = "linux")]
async fn verify_delegated_monitor_cleanup(owner_exit: MonitorOwnerExit) {
    use rustix::process::{waitid, WaitId, WaitIdOptions};
    use std::os::fd::AsFd;

    // Observe waitability without consuming any result from the monitor's owner.
    let observe = WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT;
    let finish = Arc::new(tokio::sync::Notify::new());
    let fixture = fixture_with_model(MockModel {
        index: Arc::new(AtomicUsize::new(0)),
        background: json!({
            "command": "printf '%s\\n' \"$$\" > monitor.pid; sleep 60 & printf '%s\\n' \"$!\" > descendant.pid; wait",
            "description": "delegated monitor",
            "shell": "sh",
            "timeout_ms": 1_000,
            "max_runtime_ms": 60_000,
        }),
        fail_after_background: false,
        monitor_finish: Some(finish.clone()),
    })
    .await;
    let executor = fixture.executor();
    let mut owners = tokio::task::JoinSet::new();
    let owner = owners.spawn(async move {
        executor
            .execute_delegated(&json!({"prompt":"start a monitor","agent_type":"general"}))
            .await
    });

    // The script can publish a PID only after TaskCreate committed the row
    // and its schema. A SQLite filename alone can appear before either.
    let mut processes = Vec::new();
    for path in ["monitor.pid", "descendant.pid"] {
        let pid = wait_for_pid(&fixture.temp.path().join(path)).await;
        let process = rustix::process::Pid::from_raw(pid as i32).unwrap();
        let fd =
            rustix::process::pidfd_open(process, rustix::process::PidfdFlags::empty()).unwrap();
        assert!(
            !process_finished(pid),
            "{path} should be alive before teardown"
        );
        if path == "monitor.pid" {
            let state = waitid(WaitId::PidFd(fd.as_fd()), observe);
            assert!(
                matches!(state, Ok(None)),
                "expected a live direct monitor child, got {state:?}"
            );
        }
        processes.push((path, tokio::io::unix::AsyncFd::new(fd).unwrap()));
    }

    let child = wait_for_child_session(&fixture.data, &fixture.parent).await;
    let id = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let events = crate::tools::monitor::collect_events(&fixture.data, &child).unwrap();
            if let Some(event) = events.into_iter().find(|event| event.kind == "renewed") {
                break event.task_id;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("delegated monitor should renew before its owner ends");
    assert_eq!(read_status(&fixture.data, &child, &id), "running");

    if owner_exit == MonitorOwnerExit::ReturnWithHistoricalStatus {
        // This spelling was writable on older versions. Keep the actual runner
        // and its pinned descendant alive while seeding its historical row.
        let db = rusqlite::Connection::open(
            fixture
                .data
                .join("data/session-tasks")
                .join(format!("{child}.sqlite3")),
        )
        .unwrap();
        assert_eq!(
            db.execute(
                "UPDATE tasks SET body=json_set(body,'$.status','in_progress') WHERE id=?1",
                [&id],
            )
            .unwrap(),
            1
        );
    }

    if owner_exit == MonitorOwnerExit::ReturnAfterTaskUpdate {
        for (mut input, expected) in [
            (json!({"status":"completed"}), "status/deletion"),
            (json!({"status":"deleted"}), "status/deletion"),
            (json!({"metadata":{"monitor":null}}), "monitor provenance"),
            (
                json!({"metadata":{"log_dir":format!("fg-{}", uuid::Uuid::new_v4())}}),
                "Monitor log_dir",
            ),
        ] {
            input["id"] = json!(id);
            let error = crate::tools::tasks::execute_for_session_async(
                "TaskUpdate",
                input,
                &fixture.data,
                &child,
            )
            .await
            .unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
        }
        crate::tools::tasks::execute_for_session_async(
            "TaskUpdate",
            json!({"id":id,"description":"owner annotation","metadata":{"note":"kept"}}),
            &fixture.data,
            &child,
        )
        .await
        .unwrap();
        let task = crate::tools::tasks::read_for_session(&id, &fixture.data, &child).unwrap();
        assert_eq!(task["description"], "owner annotation");
        assert_eq!(task["metadata"]["note"], "kept");
        assert_eq!(task["status"], "running");
        assert_eq!(task["metadata"]["monitor"], true);
        assert!(task["metadata"].get("log_dir").is_none());
    }

    if owner_exit == MonitorOwnerExit::Abort {
        owner.abort();
    } else {
        finish.notify_one();
    }
    let joined = tokio::time::timeout(Duration::from_secs(15), owners.join_next())
        .await
        .expect("delegated owner should finish")
        .unwrap();
    if owner_exit == MonitorOwnerExit::Abort {
        assert!(joined.unwrap_err().is_cancelled());
    } else {
        let result = joined.unwrap().unwrap();
        assert_eq!(result["session_id"], child);
        assert_eq!(result["status"], "completed", "{result}");
        assert!(
            result["result"].as_str().unwrap().contains(
                "Stopped 1 background task(s) the subagent left running: delegated monitor."
            ),
            "{result}"
        );
        assert_eq!(read_status(&fixture.data, &child, &id), "cancelled");
    }

    wait_for_status(&fixture.data, &child, &id, "cancelled").await;
    for (path, process) in processes {
        let _ready = tokio::time::timeout(Duration::from_secs(10), process.readable())
            .await
            .unwrap_or_else(|_| panic!("{path} stayed alive after delegated owner teardown"))
            .unwrap();
        if path == "monitor.pid" {
            let state = waitid(WaitId::PidFd(process.get_ref().as_fd()), observe);
            assert!(
                matches!(state, Err(rustix::io::Errno::CHILD)),
                "monitor child was not reaped: {state:?}"
            );
        }
    }
    let events = crate::tools::monitor::collect_events(&fixture.data, &child).unwrap();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].task_id, id);
    assert_eq!(events[0].kind, "stopped");
    fixture.server.abort();
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_renewing_monitor_stops_when_its_subagent_finishes() {
    verify_delegated_monitor_cleanup(MonitorOwnerExit::Return).await;
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_renewing_monitor_stops_when_its_parent_aborts() {
    verify_delegated_monitor_cleanup(MonitorOwnerExit::Abort).await;
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_renewing_monitor_cannot_be_hidden_from_delegated_cleanup_with_task_update() {
    verify_delegated_monitor_cleanup(MonitorOwnerExit::ReturnAfterTaskUpdate).await;
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delegated_cleanup_stops_a_surviving_in_progress_monitor() {
    verify_delegated_monitor_cleanup(MonitorOwnerExit::ReturnWithHistoricalStatus).await;
}
