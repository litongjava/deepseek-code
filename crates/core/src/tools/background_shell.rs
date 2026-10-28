//! Managed shell tasks share durable session task records and explicit cancellation.
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{
    future::Future,
    path::{Path, PathBuf},
    time::Duration,
};
use tokio::process::Command;
#[path = "shell_notifications.rs"]
mod notifications;
pub use notifications::collect_completed;
tokio::task_local! { static CONTEXT: Option<(PathBuf, String)>; }

/// A shell task's log directory: its task ID, or the directory of a
/// foreground call that was moved to the background.
fn directory(root: &Path, session: &str, task: &Value) -> Result<PathBuf> {
    let session = uuid::Uuid::parse_str(session)?.to_string();
    let base = root.join("shell-tasks").join(session);
    if let Some(name) = task["metadata"]["log_dir"].as_str() {
        anyhow::ensure!(
            name.starts_with("fg-") && uuid::Uuid::parse_str(&name[3..]).is_ok(),
            "Invalid shell log directory"
        );
        return Ok(base.join(name));
    }
    let id = uuid::Uuid::parse_str(task["id"].as_str().context("Task ID missing")?)?.to_string();
    Ok(base.join(id))
}

/// Shared status presentation without loading process logs.
pub fn task_status(root: &Path, session: &str, task: &Value) -> Result<String> {
    let fresh = std::fs::metadata(directory(root, session, task)?.join("heartbeat"))
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|time| time.elapsed().ok())
        .is_some_and(|age| age < Duration::from_secs(10));
    Ok(if task["status"] == "running" && !fresh {
        "unresponsive".into()
    } else {
        task["status"].as_str().unwrap_or("unknown").to_owned()
    })
}

pub(crate) async fn scope<F: Future>(context: Option<(PathBuf, String)>, future: F) -> F::Output {
    CONTEXT.scope(context, future).await
}

pub(super) fn session_environment() -> Result<Option<String>> {
    match CONTEXT.try_with(Clone::clone).ok().flatten() {
        Some((root, session)) => crate::services::session_environment::script(&root, &session),
        None => Ok(None),
    }
}

static ACTIVE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// Only tasks owned by this process keep its persistent worker alive.
pub fn has_active() -> bool {
    ACTIVE.load(std::sync::atomic::Ordering::Acquire) != 0
}

struct Completion {
    root: PathBuf,
    session: String,
    id: String,
    done: bool,
}
impl Completion {
    fn finish(&mut self, status: &str, output: Value) {
        let result = super::tasks::execute_for_shell_owner(
            "TaskUpdate",
            json!({"id":self.id,"status":status,"output":output.to_string()}),
            &self.root,
            &self.session,
        );
        self.done = result.is_ok();
    }
}
impl Drop for Completion {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        if !self.done {
            self.finish(
                "cancelled",
                json!({"is_error":true,"error":"Shell owner exited or task was cancelled"}),
            );
        }
    }
}

pub(super) fn start(
    mut command: Command,
    timeout: Option<Duration>,
    shell: &str,
    description: &str,
    command_text: &str,
) -> Result<Value> {
    let (root, session) = CONTEXT
        .try_with(Clone::clone)
        .ok()
        .flatten()
        .context("Background Bash requires an owning engine session")?;
    let task = super::tasks::execute_for_shell_owner(
        "TaskCreate",
        // The command and start time are kept for the task details view.
        json!({"subject":description,"metadata":{"managed_shell":true,"command":command_text,"started_ms":chrono::Utc::now().timestamp_millis()}}),
        &root,
        &session,
    )?;
    let id = task["id"].as_str().context("Task ID missing")?.to_string();
    let directory = root.join("shell-tasks").join(&session).join(&id);
    std::fs::create_dir_all(&directory)?;
    let stdout = directory.join("stdout.log");
    let stderr = directory.join("stderr.log");
    super::tasks::execute_for_shell_owner(
        "TaskUpdate",
        json!({"id":id,"status":"running"}),
        &root,
        &session,
    )?;
    std::fs::write(directory.join("heartbeat"), b"running")?;
    let shell = shell.to_string();
    let logs = (stdout.clone(), stderr.clone());
    ACTIVE.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    let mut completion = Completion {
        root,
        session,
        id: id.clone(),
        done: false,
    };
    tokio::spawn(async move {
        let result = tokio::select! {
            result = super::bash_process::run_logged(&mut command, timeout, &shell, Some(logs)) => result,
            _ = async { let mut heartbeat = std::time::Instant::now(); loop { if directory.join("cancel").exists() { break; } if heartbeat.elapsed() >= Duration::from_secs(1) { let _ = std::fs::write(directory.join("heartbeat"), b"running"); heartbeat = std::time::Instant::now(); } tokio::time::sleep(Duration::from_millis(50)).await; } } => {
                completion.finish("cancelled", json!({"is_error":true,"cancelled":true}));
                return;
            }
        };
        let output =
            result.unwrap_or_else(|error| json!({"is_error":true,"error":format!("{error:#}")}));
        let status = if output["success"] == true {
            "completed"
        } else {
            "failed"
        };
        completion.finish(status, output);
    });
    Ok(
        json!({"task_id":id,"status":"running","stdout_file":stdout,"stderr_file":stderr,"message":"Managed background command started. Use TaskOutput with task_id for current logs, TaskStop to terminate it. It runs until completion, timeout, cancellation, or worker exit."}),
    )
}

/// Asks this session's running foreground Bash calls to continue in the
/// background, as Ctrl+B does in the reference CLI. Calls started later
/// ignore the request.
pub fn request_background(root: &Path, session: &str) -> Result<()> {
    let directory = root.join("shell-tasks").join(uuid::Uuid::parse_str(session)?.to_string());
    std::fs::create_dir_all(&directory)?;
    std::fs::write(directory.join("background-now"), b"1")?;
    Ok(())
}

/// Aborts a foreground call's process when the turn drops it.
struct AbortOnDrop(Option<tokio::task::JoinHandle<Result<Value>>>);
impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            handle.abort();
        }
    }
}

/// A foreground Bash call that the user can move to the background while it
/// runs; it then continues as a managed shell task with the same logs.
pub(super) async fn run_foreground(
    mut command: Command,
    timeout: Duration,
    shell: &str,
    description: &str,
    command_text: &str,
) -> Result<Value> {
    let Some((root, session)) = CONTEXT.try_with(Clone::clone).ok().flatten() else {
        return super::bash_process::run(&mut command, timeout, shell).await;
    };
    let session_directory = root.join("shell-tasks").join(uuid::Uuid::parse_str(&session)?.to_string());
    let log_dir = format!("fg-{}", uuid::Uuid::new_v4());
    let directory = session_directory.join(&log_dir);
    std::fs::create_dir_all(&directory)?;
    let signal = session_directory.join("background-now");
    let started = std::time::SystemTime::now();
    let started_ms = chrono::Utc::now().timestamp_millis();
    let logs = (directory.join("stdout.log"), directory.join("stderr.log"));
    let shell_name = shell.to_owned();
    let mut running = AbortOnDrop(Some(tokio::spawn(async move {
        super::bash_process::run_logged(&mut command, Some(timeout), &shell_name, Some(logs)).await
    })));
    loop {
        let handle = running.0.as_mut().context("Shell task missing")?;
        tokio::select! {
            result = handle => {
                running.0 = None;
                let _ = std::fs::remove_dir_all(&directory);
                return result.context("Shell task stopped")?;
            }
            _ = tokio::time::sleep(Duration::from_millis(100)) => {
                let requested = std::fs::metadata(&signal)
                    .and_then(|metadata| metadata.modified())
                    .is_ok_and(|time| time >= started);
                if requested {
                    break;
                }
            }
        }
    }
    let task = super::tasks::execute_for_shell_owner(
        "TaskCreate",
        json!({"subject":description,"metadata":{"managed_shell":true,"command":command_text,"started_ms":started_ms,"log_dir":log_dir}}),
        &root,
        &session,
    )?;
    let id = task["id"].as_str().context("Task ID missing")?.to_string();
    // The task ID directory marks a managed shell for blocking TaskOutput
    // waits; the logs stay where the call started.
    std::fs::create_dir_all(session_directory.join(&id))?;
    super::tasks::execute_for_shell_owner("TaskUpdate", json!({"id":id,"status":"running"}), &root, &session)?;
    std::fs::write(directory.join("heartbeat"), b"running")?;
    ACTIVE.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    let mut completion = Completion { root, session, id: id.clone(), done: false };
    let mut handle = running.0.take().context("Shell task missing")?;
    tokio::spawn(async move {
        let result = tokio::select! {
            result = &mut handle => result,
            _ = async { let mut heartbeat = std::time::Instant::now(); loop { if directory.join("cancel").exists() { break; } if heartbeat.elapsed() >= Duration::from_secs(1) { let _ = std::fs::write(directory.join("heartbeat"), b"running"); heartbeat = std::time::Instant::now(); } tokio::time::sleep(Duration::from_millis(50)).await; } } => {
                handle.abort();
                completion.finish("cancelled", json!({"is_error":true,"cancelled":true}));
                return;
            }
        };
        let output = match result {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => json!({"is_error":true,"error":format!("{error:#}")}),
            Err(error) => json!({"is_error":true,"error":format!("{error}")}),
        };
        let status = if output["success"] == true { "completed" } else { "failed" };
        completion.finish(status, output);
    });
    Ok(json!({"task_id":id,"status":"running","backgrounded":true,"message":"The user moved this command to the background; it keeps running. Use TaskOutput with task_id for its output and TaskStop to stop it."}))
}

pub(super) fn control(
    name: &str,
    input: &Value,
    root: &Path,
    session: &str,
) -> Result<Option<Value>> {
    if !matches!(name, "TaskOutput" | "TaskStop") {
        return Ok(None);
    }
    let policy = if name == "TaskOutput" {
        Some(wait_policy(input)?)
    } else {
        anyhow::ensure!(
            input.get("block").is_none() && input.get("timeout").is_none(),
            "TaskStop does not accept block or timeout"
        );
        None
    };
    let id = input["task_id"]
        .as_str()
        .or_else(|| input["taskId"].as_str())
        .or_else(|| input["id"].as_str())
        .context("task_id is required")?;
    let task = super::tasks::execute_for_session("TaskGet", json!({"id":id}), root, session)?;
    if task["metadata"]["managed_shell"] != true {
        return Ok(None);
    }
    anyhow::ensure!(policy.is_none_or(|(block, _)| !block), "Blocking TaskOutput requires the asynchronous task entry point; synchronous callers must set block:false");
    let id = uuid::Uuid::parse_str(id)?.to_string();
    let session = uuid::Uuid::parse_str(session)?;
    let directory = directory(root, &session.to_string(), &task)?;
    let status = json!(task_status(root, &session.to_string(), &task)?);
    if name == "TaskStop" {
        let active = task["status"] == "running";
        if active {
            std::fs::write(directory.join("cancel"), b"cancel")?;
        }
        return Ok(Some(
            json!({"task_id":id,"status":status,"cancellation_requested":active,"message":"Cancellation is requested, not confirmed. TaskOutput reports terminal status after the owner terminates the shell process tree; an unresponsive owner may not have received it yet."}),
        ));
    }
    let read = |name| -> Result<String> {
        use std::io::Read;
        match std::fs::File::open(directory.join(name)) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(1024 * 1024).read_to_end(&mut bytes)?;
                Ok(String::from_utf8_lossy(&bytes).into_owned())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(error) => Err(error.into()),
        }
    };
    Ok(Some(
        json!({"task_id":id,"status":status,"stdout":read("stdout.log")?,"stderr":read("stderr.log")?,"result":task["output"],"block":false,"retrieval_status":if terminal(&status) {"success"} else {"not_ready"}}),
    ))
}

fn terminal(status: &Value) -> bool {
    matches!(status.as_str(), Some("completed" | "failed" | "cancelled"))
}

fn wait_policy(input: &Value) -> Result<(bool, Duration)> {
    let block = input
        .get("block")
        .map(|v| v.as_bool().context("block must be boolean"))
        .transpose()?
        .unwrap_or(true);
    let timeout = input
        .get("timeout")
        .map(|v| {
            v.as_u64()
                .context("TaskOutput timeout must be an integer number of milliseconds")
        })
        .transpose()?
        .unwrap_or(30_000);
    anyhow::ensure!(
        timeout <= 600_000,
        "TaskOutput timeout must be 0..600000 milliseconds"
    );
    Ok((block, Duration::from_millis(timeout)))
}

pub(super) async fn control_async(
    name: &str,
    input: &Value,
    root: &Path,
    session: &str,
) -> Result<Option<Value>> {
    if name != "TaskOutput" {
        return Ok(None);
    }
    let (block, timeout) = wait_policy(input)?;
    let deadline = tokio::time::Instant::now() + timeout;
    let mut snapshot_input = input.clone();
    snapshot_input["block"] = json!(false);
    let id = input["task_id"]
        .as_str()
        .or_else(|| input["taskId"].as_str())
        .or_else(|| input["id"].as_str())
        .context("task_id is required")?;
    let id = uuid::Uuid::parse_str(id)?.to_string();
    let session_id = uuid::Uuid::parse_str(session)?.to_string();
    // Moved foreground calls also get this directory, as a marker.
    let managed_directory = root
        .join("shell-tasks")
        .join(&session_id)
        .join(&id)
        .is_dir();
    let mut last = json!({"task_id":id,"status":"unknown","stdout":"","stderr":"","result":null,"block":block,"retrieval_status":"timeout"});
    loop {
        let (root, session, input) = (root.to_owned(), session.to_owned(), snapshot_input.clone());
        let snapshot =
            tokio::task::spawn_blocking(move || control("TaskOutput", &input, &root, &session));
        let snapshot = if block && managed_directory {
            match tokio::time::timeout_at(deadline, snapshot).await {
                Ok(result) => result??,
                Err(_) => {
                    last["retrieval_status"] = json!("timeout");
                    return Ok(Some(last));
                }
            }
        } else {
            snapshot.await??
        };
        let Some(mut value) = snapshot else {
            return Ok(None);
        };
        value["block"] = json!(block);
        if terminal(&value["status"]) || !block {
            return Ok(Some(value));
        }
        if tokio::time::Instant::now() >= deadline {
            value["retrieval_status"] = json!("timeout");
            return Ok(Some(value));
        }
        last = value;
        tokio::time::sleep_until(
            deadline.min(tokio::time::Instant::now() + Duration::from_millis(50)),
        )
        .await;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn waiting_fixture(root: &Path, session: &str) -> String {
        let task = super::super::tasks::execute_for_shell_owner(
            "TaskCreate",
            json!({"subject":"waiting fixture","metadata":{"managed_shell":true}}),
            root,
            session,
        )
        .unwrap();
        let id = task["id"].as_str().unwrap().to_owned();
        super::super::tasks::execute_for_shell_owner(
            "TaskUpdate",
            json!({"id":id,"status":"running"}),
            root,
            session,
        )
        .unwrap();
        let directory = root.join("shell-tasks").join(session).join(&id);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("heartbeat"), b"alive").unwrap();
        std::fs::write(directory.join("stdout.log"), b"partial output").unwrap();
        id
    }
    #[test]
    fn output_wait_policy_uses_milliseconds_and_rejects_invalid_values() {
        assert_eq!(
            wait_policy(&json!({})).unwrap(),
            (true, Duration::from_secs(30))
        );
        assert_eq!(
            wait_policy(&json!({"block":false,"timeout":1})).unwrap(),
            (false, Duration::from_millis(1))
        );
        for input in [
            json!({"block":"true"}),
            json!({"timeout":-1}),
            json!({"timeout":1.5}),
            json!({"timeout":600001}),
        ] {
            assert!(wait_policy(&input).is_err());
        }
    }
    #[tokio::test]
    async fn output_wait_timeout_snapshot_and_session_isolation() {
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let id = waiting_fixture(root.path(), &session);
        let snapshot = super::super::tasks::execute_for_session_async(
            "TaskOutput",
            json!({"task_id":id,"block":false}),
            root.path(),
            &session,
        )
        .await
        .unwrap();
        assert_eq!(snapshot["retrieval_status"], "not_ready");
        for timeout in [0, 40] {
            let started = tokio::time::Instant::now();
            let value = super::super::tasks::execute_for_session_async(
                "TaskOutput",
                json!({"task_id":id,"timeout":timeout}),
                root.path(),
                &session,
            )
            .await
            .unwrap();
            assert_eq!(value["retrieval_status"], "timeout");
            if timeout != 0 {
                assert_eq!(value["status"], "running");
                assert_eq!(value["stdout"], "partial output");
            }
            assert!(started.elapsed() >= Duration::from_millis(timeout));
        }
        assert!(super::super::tasks::execute_for_session_async(
            "TaskOutput",
            json!({"task_id":id,"block":false}),
            root.path(),
            &uuid::Uuid::new_v4().to_string()
        )
        .await
        .is_err());
        assert!(!root
            .path()
            .join("shell-tasks")
            .join(&session)
            .join(&id)
            .join("cancel")
            .exists());
    }
    #[tokio::test]
    async fn output_deadline_bounds_locked_database_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let id = waiting_fixture(root.path(), &session);
        let lock = rusqlite::Connection::open(
            root.path().join("tasks").join(format!("{session}.sqlite3")),
        )
        .unwrap();
        lock.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let request = json!({"task_id":id,"timeout":40});
        let value = tokio::time::timeout(
            Duration::from_millis(500),
            control_async("TaskOutput", &request, root.path(), &session),
        )
        .await;
        lock.execute_batch("ROLLBACK").unwrap();
        let value = value
            .expect("SQLite busy wait exceeded TaskOutput deadline")
            .unwrap()
            .unwrap();
        assert_eq!(value["retrieval_status"], "timeout");
        assert_eq!(value["status"], "unknown");
    }
    #[tokio::test]
    async fn output_wait_observes_completion_and_cancels_without_stopping_shell() {
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let id = waiting_fixture(root.path(), &session);
        let update = async {
            tokio::time::sleep(Duration::from_millis(70)).await;
            super::super::tasks::execute_for_shell_owner(
                "TaskUpdate",
                json!({"id":id,"status":"completed"}),
                root.path(),
                &session,
            )
            .unwrap();
        };
        let request = json!({"task_id":id});
        let (value, _) = tokio::join!(
            control_async("TaskOutput", &request, root.path(), &session),
            update
        );
        assert_eq!(value.unwrap().unwrap()["retrieval_status"], "success");
        let id = waiting_fixture(root.path(), &session);
        let request = json!({"task_id":id});
        assert!(tokio::time::timeout(
            Duration::from_millis(80),
            control_async("TaskOutput", &request, root.path(), &session)
        )
        .await
        .is_err());
        assert!(!root
            .path()
            .join("shell-tasks")
            .join(&session)
            .join(&id)
            .join("cancel")
            .exists());
        let snapshot = control(
            "TaskOutput",
            &json!({"task_id":id,"block":false}),
            root.path(),
            &session,
        )
        .unwrap()
        .unwrap();
        assert_eq!(snapshot["status"], "running");
    }
    #[test]
    fn unresponsive_owner_receives_cancel_and_sync_blocking_wait_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let task = super::super::tasks::execute_for_shell_owner(
            "TaskCreate",
            json!({"subject":"stale shell","metadata":{"managed_shell":true}}),
            root.path(),
            &session,
        )
        .unwrap();
        let id = task["id"].as_str().unwrap();
        super::super::tasks::execute_for_shell_owner(
            "TaskUpdate",
            json!({"id":id,"status":"running"}),
            root.path(),
            &session,
        )
        .unwrap();
        let directory = root.path().join("shell-tasks").join(&session).join(id);
        std::fs::create_dir_all(&directory).unwrap();
        let stopped = control("TaskStop", &json!({"task_id":id}), root.path(), &session)
            .unwrap()
            .unwrap();
        assert_eq!(stopped["status"], "unresponsive");
        assert_eq!(stopped["cancellation_requested"], true);
        assert!(directory.join("cancel").exists());
        assert!(control(
            "TaskOutput",
            &json!({"task_id":id,"block":true}),
            root.path(),
            &session
        )
        .is_err());
    }
    async fn output(root: &Path, session: &str, id: &str, terminal: bool) -> Value {
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let value = super::super::tasks::execute_for_session(
                    "TaskOutput",
                    json!({"task_id":id,"block":false}),
                    root,
                    session,
                )
                .unwrap();
                if (!terminal && value["stdout"].as_str().unwrap().contains("ready"))
                    || (terminal && value["status"] != "running")
                {
                    return value;
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        })
        .await
        .unwrap()
    }
    #[tokio::test]
    async fn a_running_foreground_call_moves_to_the_background_on_request() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let script = "console.log('early');setTimeout(()=>console.log('late'),900)";
        // Without a request the call finishes in the foreground as before.
        let mut quick = Command::new("node");
        quick.arg("-e").arg("console.log('quick')");
        let done = scope(Some((root.path().into(), session.clone())), run_foreground(quick, Duration::from_secs(30), "node", "quick", "node quick")).await.unwrap();
        assert!(done["stdout"].as_str().unwrap().contains("quick"));
        let mut command = Command::new("node");
        command.arg("-e").arg(script);
        let (root_path, id) = (root.path().to_owned(), session.clone());
        let moved = scope(Some((root.path().into(), session.clone())), async move {
            let request = async {
                tokio::time::sleep(Duration::from_millis(400)).await;
                request_background(&root_path, &id).unwrap();
            };
            let (result, ()) = tokio::join!(run_foreground(command, Duration::from_secs(30), "node", "slow", "node slow"), request);
            result
        })
        .await
        .unwrap();
        assert_eq!(moved["backgrounded"], true);
        let id = moved["task_id"].as_str().unwrap();
        let finished = output(root.path(), &session, id, true).await;
        assert_eq!(finished["status"], "completed");
        let stdout = finished["stdout"].as_str().unwrap();
        assert!(stdout.contains("early") && stdout.contains("late"), "{stdout}");
        // A request made before a call starts does not move it.
        let mut later = Command::new("node");
        later.arg("-e").arg("setTimeout(()=>console.log('kept'),300)");
        let kept = scope(Some((root.path().into(), session.clone())), run_foreground(later, Duration::from_secs(30), "node", "later", "node later")).await.unwrap();
        assert!(kept["stdout"].as_str().unwrap().contains("kept"));
    }

    #[tokio::test]
    async fn managed_shell_survives_turn_returns_live_logs_and_cancels_descendants() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let leaked = root.path().join("leaked");
        let mut command = Command::new("node");
        command.arg("-e").arg("require('child_process').spawn(process.execPath,['-e',`setTimeout(()=>require('fs').writeFileSync(process.argv[1],'leaked'),1800)`,process.argv[1]],{stdio:'inherit'});console.log('ready');console.error('diagnostic');setInterval(()=>{},10000)").arg(&leaked);
        let task = scope(Some((root.path().into(), session.clone())), async {
            start(command, None, "node", "test managed shell", "node fixture").unwrap()
        })
        .await;
        let id = task["task_id"].as_str().unwrap();
        let running = output(root.path(), &session, id, false).await;
        assert_eq!(running["status"], "running");
        assert!(running["stderr"].as_str().unwrap().contains("diagnostic"));
        for change in [json!({"status":"completed"}),json!({"status":"deleted"}),json!({"output":"fabricated"}),json!({"metadata":{"managed_shell":false}}),json!({"metadata":{"managed_shell":null}})] {
            let mut change=change;change["id"]=json!(id);
            assert!(super::super::tasks::execute_for_session("TaskUpdate",change,root.path(),&session).is_err());
        }
        super::super::tasks::execute_for_session("TaskUpdate",json!({"id":id,"subject":"renamed server","metadata":{"note":"safe user annotation"}}),root.path(),&session).unwrap();
        let still_running=output(root.path(),&session,id,false).await;
        assert_eq!(still_running["status"],"running");
        assert!(super::super::tasks::execute_for_session("TaskCreate",json!({"subject":"fake managed task","metadata":{"managed_shell":true}}),root.path(),&session).is_err());
        assert!(super::super::tasks::execute_for_session(
            "TaskStop",
            json!({"task_id":id}),
            root.path(),
            &uuid::Uuid::new_v4().to_string()
        )
        .is_err());
        let stopped = super::super::tasks::execute_for_session(
            "TaskStop",
            json!({"task_id":id}),
            root.path(),
            &session,
        )
        .unwrap();
        assert_eq!(stopped["cancellation_requested"], true);
        assert_eq!(
            output(root.path(), &session, id, true).await["status"],
            "cancelled"
        );
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(
            !leaked.exists(),
            "background descendant escaped cancellation"
        );
    }
    #[tokio::test]
    async fn managed_shell_completion_and_timeout_preserve_output() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        for (code, expected) in [
            ("console.log('finished')", "completed"),
            ("console.log('partial');setInterval(()=>{},10000)", "failed"),
        ] {
            let mut command = Command::new("node");
            command.args(["-e", code]);
            let task = scope(Some((root.path().into(), session.clone())), async {
                start(command, Some(Duration::from_millis(600)), "node", "fixture", "node fixture").unwrap()
            })
            .await;
            let observed = output(
                root.path(),
                &session,
                task["task_id"].as_str().unwrap(),
                true,
            )
            .await;
            assert_eq!(observed["status"], expected);
            assert!(!observed["stdout"].as_str().unwrap().is_empty());
            if expected == "failed" {
                assert!(observed["result"].as_str().unwrap().contains("timed_out"));
            }
        }
    }
}
