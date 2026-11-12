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
pub use notifications::{
    collect_completed, collect_finished, notification_summary, Finished, NOTIFICATION_START,
};
tokio::task_local! { pub(super) static CONTEXT: Option<(PathBuf, String)>; }

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

/// Runs a task with its owning session; used by the Monitor tool and the CLI
/// tests.
pub async fn scope<F: Future>(context: Option<(PathBuf, String)>, future: F) -> F::Output {
    CONTEXT.scope(context, future).await
}

pub(super) fn session_environment() -> Result<Option<String>> {
    match CONTEXT.try_with(Clone::clone).ok().flatten() {
        Some((root, session)) => crate::services::session_environment::script(&root, &session),
        None => Ok(None),
    }
}

pub(super) fn shell_settings() -> Result<crate::services::shell_settings::ShellSettings> {
    let root = match CONTEXT.try_with(Clone::clone).ok().flatten() {
        Some((root, _)) => root,
        None => crate::runtime::resolve_default_data_dir()?,
    };
    crate::services::shell_settings::load(&root)
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

pub(crate) fn start(
    mut command: Command,
    timeout: Option<Duration>,
    shell: &str,
    description: &str,
    command_text: &str,
    guard: Option<crate::services::wsl::Guard>,
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
        let _guard = guard;
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
        json!({"task_id":id,"subject":description,"status":"running","stdout_file":stdout,"stderr_file":stderr,"message":format!("Background command \"{description}\" started. You will be notified when it finishes. To read its output earlier, load TaskGet with ToolSearch; TaskStop stops it.")}),
    )
}

/// Asks this session's running foreground Bash calls to continue in the
/// background, as Ctrl+B does in the reference CLI. Calls started later
/// ignore the request.
pub fn request_background(root: &Path, session: &str) -> Result<()> {
    let directory = root
        .join("shell-tasks")
        .join(uuid::Uuid::parse_str(session)?.to_string());
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

/// Read bounded output from foreground calls owned by this session.
/// Background calls are excluded by their absent active marker.
pub fn foreground_output(root: &Path, session: &str) -> Result<String> {
    let base = root
        .join("shell-tasks")
        .join(uuid::Uuid::parse_str(session)?.to_string());
    let mut output = String::new();
    let Ok(entries) = std::fs::read_dir(base) else {
        return Ok(output);
    };
    for entry in entries.flatten() {
        let directory = entry.path();
        if !directory.join("foreground-active").exists() {
            continue;
        }
        if let Ok(command) = std::fs::read_to_string(directory.join("command.txt")) {
            output.push_str(&format!("{command}\n"));
        }
        for stream in ["stdout", "stderr"] {
            use std::io::{Read, Seek, SeekFrom};
            if let Ok(mut file) = std::fs::File::open(directory.join(format!("{stream}.log"))) {
                let length = file.metadata()?.len();
                file.seek(SeekFrom::Start(length.saturating_sub(32768)))?;
                let mut bytes = Vec::new();
                file.take(32768).read_to_end(&mut bytes)?;
                if !bytes.is_empty() {
                    output.push_str(&format!(
                        "[{stream}]\n{}\n",
                        String::from_utf8_lossy(&bytes)
                    ));
                }
            }
        }
    }
    Ok(output)
}
struct ForegroundMarker(PathBuf);
impl Drop for ForegroundMarker {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

pub(super) async fn run_foreground(
    mut command: Command,
    timeout: Duration,
    shell: &str,
    description: &str,
    command_text: &str,
    guard: Option<crate::services::wsl::Guard>,
) -> Result<Value> {
    // Managed shells belong to a UUID session; anything else runs as before.
    let Some((root, session, id)) =
        CONTEXT
            .try_with(Clone::clone)
            .ok()
            .flatten()
            .and_then(|(root, session)| {
                uuid::Uuid::parse_str(&session)
                    .ok()
                    .map(|id| (root, session, id))
            })
    else {
        let _guard = guard;
        return super::bash_process::run(&mut command, timeout, shell).await;
    };
    let session_directory = root.join("shell-tasks").join(id.to_string());
    let log_dir = format!("fg-{}", uuid::Uuid::new_v4());
    let directory = session_directory.join(&log_dir);
    std::fs::create_dir_all(&directory)?;
    std::fs::write(directory.join("command.txt"), command_text)?;
    let marker = directory.join("foreground-active");
    std::fs::write(&marker, b"running")?;
    let _foreground_marker = ForegroundMarker(marker);
    let signal = session_directory.join("background-now");
    let started = std::time::SystemTime::now();
    let started_ms = chrono::Utc::now().timestamp_millis();
    let logs = (directory.join("stdout.log"), directory.join("stderr.log"));
    let shell_name = shell.to_owned();
    let mut running = AbortOnDrop(Some(tokio::spawn(async move {
        let _guard = guard;
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
    // The task ID directory marks a managed shell for blocking TaskGet waits;
    // the logs stay where the call started.
    std::fs::create_dir_all(session_directory.join(&id))?;
    super::tasks::execute_for_shell_owner(
        "TaskUpdate",
        json!({"id":id,"status":"running"}),
        &root,
        &session,
    )?;
    std::fs::write(directory.join("heartbeat"), b"running")?;
    ACTIVE.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    let mut completion = Completion {
        root,
        session,
        id: id.clone(),
        done: false,
    };
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
        let status = if output["success"] == true {
            "completed"
        } else {
            "failed"
        };
        completion.finish(status, output);
    });
    Ok(
        json!({"task_id":id,"status":"running","backgrounded":true,"message":"The user moved this command to the background; it keeps running. You will be notified when it finishes. To read its output earlier, load TaskGet with ToolSearch; TaskStop stops it."}),
    )
}

pub(super) fn control(
    name: &str,
    input: &Value,
    root: &Path,
    session: &str,
) -> Result<Option<Value>> {
    if !matches!(name, "TaskGet" | "TaskOutput" | "TaskStop") {
        return Ok(None);
    }
    let policy = if matches!(name, "TaskGet" | "TaskOutput") {
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
        .or_else(|| input["shell_id"].as_str())
        .context("task_id is required")?;
    // Read the record directly: calling back through TaskGet would re-enter
    // this control function and recurse for managed shells.
    let task = super::tasks::read_for_session(id, root, session)?;
    // A monitor is a managed background task too, but it ends through its
    // event stream rather than a shell completion record.
    let monitor = task["metadata"]["monitor"] == true;
    if task["metadata"]["managed_shell"] != true && !monitor {
        return Ok(None);
    }
    anyhow::ensure!(policy.is_none_or(|(block, _)| !block), "Blocking TaskGet requires the asynchronous task entry point; synchronous callers must set block:false");
    let id = uuid::Uuid::parse_str(id)?.to_string();
    let session = uuid::Uuid::parse_str(session)?;
    let directory = directory(root, &session.to_string(), &task)?;
    // Only a shell with a real task directory has live logs and a heartbeat.
    // A synthetic or imported record falls back to its stored fields.
    if policy.is_some() && !directory.exists() {
        return Ok(None);
    }
    let status = json!(task_status(root, &session.to_string(), &task)?);
    if monitor {
        if name == "TaskStop" {
            // The monitor loop checks this file every 50 ms.
            std::fs::create_dir_all(&directory)?;
            std::fs::write(directory.join("cancel"), b"cancel")?;
            return Ok(Some(json!({"task_id":id,"status":status,"cancellation_requested":true,"message":"Monitor stop requested; you will get a final 'stopped' notification."})));
        }
        let stdout = read_tail(&directory.join("stdout.log"), 64 * 1024)?;
        let events = std::fs::File::open(directory.join("events.jsonl"))
            .ok()
            .map(|file| {
                std::io::BufRead::lines(std::io::BufReader::new(file))
                    .filter(|line| line.is_ok())
                    .count()
            })
            .unwrap_or(0);
        let mut snapshot = task;
        snapshot["task_id"] = json!(id);
        snapshot["status"] = status;
        snapshot["stdout"] = json!(stdout);
        snapshot["events"] = json!(events);
        return Ok(Some(snapshot));
    }
    if name == "TaskStop" {
        let active = task["status"] == "running";
        if active {
            std::fs::write(directory.join("cancel"), b"cancel")?;
        }
        return Ok(Some(
            json!({"task_id":id,"subject":task["subject"],"status":status,"cancellation_requested":active,"message":"Cancellation is requested, not confirmed. TaskGet reports terminal status after the owner terminates the shell process tree; an unresponsive owner may not have received it yet."}),
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
    // TaskGet keeps the stored task fields (description, owner, metadata,
    // dependencies, output) and adds the live status and bounded logs.
    let stdout = read("stdout.log")?;
    let stderr = read("stderr.log")?;
    let mut snapshot = task;
    snapshot["task_id"] = json!(id);
    snapshot["status"] = status.clone();
    snapshot["stdout"] = json!(stdout);
    snapshot["stderr"] = json!(stderr);
    let result = snapshot["output"].clone();
    snapshot["result"] = result;
    snapshot["block"] = json!(false);
    snapshot["retrieval_status"] = json!(if terminal(&status) {
        "success"
    } else {
        "not_ready"
    });
    Ok(Some(snapshot))
}

/// Reads at most `limit` bytes from the end of a log file.
fn read_tail(path: &Path, limit: u64) -> Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    match std::fs::File::open(path) {
        Ok(mut file) => {
            let length = file.metadata()?.len();
            if length > limit {
                file.seek(SeekFrom::Start(length - limit))?;
            }
            let mut bytes = Vec::new();
            file.take(limit).read_to_end(&mut bytes)?;
            Ok(String::from_utf8_lossy(&bytes).into_owned())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error.into()),
    }
}

fn terminal(status: &Value) -> bool {
    matches!(status.as_str(), Some("completed" | "failed" | "cancelled"))
}

pub(super) fn wait_policy(input: &Value) -> Result<(bool, Duration)> {
    let block = input
        .get("block")
        .map(|v| v.as_bool().context("block must be boolean"))
        .transpose()?
        // Immediate metadata reads stay the default; waiting is explicit.
        .unwrap_or(false);
    let timeout = input
        .get("timeout")
        .map(|v| {
            v.as_u64()
                .context("TaskGet timeout must be an integer number of milliseconds")
        })
        .transpose()?
        // Only used when waiting.
        .unwrap_or(30_000);
    anyhow::ensure!(
        timeout <= 600_000,
        "TaskGet timeout must be 0..600000 milliseconds"
    );
    Ok((block, Duration::from_millis(timeout)))
}

pub(super) async fn control_async(
    name: &str,
    input: &Value,
    root: &Path,
    session: &str,
) -> Result<Option<Value>> {
    if !matches!(name, "TaskGet" | "TaskOutput") {
        return Ok(None);
    }
    let name = name.to_owned();
    let (block, timeout) = wait_policy(input)?;
    // timeout:0 reads the current snapshot without waiting.
    let wait = block && !timeout.is_zero();
    let deadline = tokio::time::Instant::now() + timeout;
    let mut snapshot_input = input.clone();
    snapshot_input["block"] = json!(false);
    let id = input["task_id"]
        .as_str()
        .or_else(|| input["taskId"].as_str())
        .or_else(|| input["id"].as_str())
        .context("task_id is required")?;
    // Managed shell and worker job IDs are UUIDs. A non-UUID id belongs to an
    // ordinary record and is read by the storage path instead.
    let Ok(id) = uuid::Uuid::parse_str(id) else {
        return Ok(None);
    };
    let id = id.to_string();
    let session_id = uuid::Uuid::parse_str(session)?.to_string();
    // Moved foreground calls also get this directory, as a marker.
    let managed_directory = root
        .join("shell-tasks")
        .join(&session_id)
        .join(&id)
        .is_dir();
    let mut last = json!({"task_id":id,"status":"unknown","stdout":"","stderr":"","result":null,"block":block,"retrieval_status":"timeout"});
    loop {
        let (root, session, input, name) = (
            root.to_owned(),
            session.to_owned(),
            snapshot_input.clone(),
            name.clone(),
        );
        let snapshot = tokio::task::spawn_blocking(move || control(&name, &input, &root, &session));
        let snapshot = if wait && managed_directory {
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
        if terminal(&value["status"]) || !wait {
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
    fn task_get_snapshot_keeps_stored_fields_and_adds_live_output() {
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let dependency = super::super::tasks::execute_for_shell_owner(
            "TaskCreate",
            json!({"subject":"dependency"}),
            root.path(),
            &session,
        )
        .unwrap();
        let task = super::super::tasks::execute_for_shell_owner(
            "TaskCreate",
            json!({"subject":"server","description":"details","metadata":{"managed_shell":true,"note":"kept"}}),
            root.path(),
            &session,
        )
        .unwrap();
        let id = task["id"].as_str().unwrap();
        super::super::tasks::execute_for_shell_owner(
            "TaskUpdate",
            json!({"id":id,"status":"running","owner":"alice","output":"{\"success\":true}","addBlockedBy":[dependency["id"]]}),
            root.path(),
            &session,
        )
        .unwrap();
        let directory = root.path().join("shell-tasks").join(&session).join(id);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("heartbeat"), b"alive").unwrap();
        std::fs::write(directory.join("stdout.log"), b"live output").unwrap();
        let snapshot = control(
            "TaskGet",
            &json!({"task_id":id,"block":false}),
            root.path(),
            &session,
        )
        .unwrap()
        .unwrap();
        assert_eq!(snapshot["id"], id);
        assert_eq!(snapshot["task_id"], id);
        assert_eq!(snapshot["subject"], "server");
        assert_eq!(snapshot["description"], "details");
        assert_eq!(snapshot["owner"], "alice");
        assert_eq!(snapshot["metadata"]["note"], "kept");
        assert_eq!(snapshot["metadata"]["managed_shell"], true);
        assert_eq!(snapshot["blockedBy"], json!([dependency["id"]]));
        assert_eq!(snapshot["output"], "{\"success\":true}");
        assert_eq!(snapshot["result"], "{\"success\":true}");
        assert_eq!(snapshot["status"], "running");
        assert_eq!(snapshot["stdout"], "live output");
        assert_eq!(snapshot["retrieval_status"], "not_ready");
    }

    #[test]
    fn output_wait_policy_uses_milliseconds_and_rejects_invalid_values() {
        assert_eq!(
            wait_policy(&json!({})).unwrap(),
            (false, Duration::from_secs(30))
        );
        assert_eq!(
            wait_policy(&json!({"block":false,"timeout":1})).unwrap(),
            (false, Duration::from_millis(1))
        );
        assert_eq!(
            wait_policy(&json!({"block":true})).unwrap(),
            (true, Duration::from_secs(30))
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
            "TaskGet",
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
                "TaskGet",
                json!({"task_id":id,"block":true,"timeout":timeout}),
                root.path(),
                &session,
            )
            .await
            .unwrap();
            if timeout == 0 {
                // A zero timeout reads the current snapshot without waiting.
                assert_eq!(value["retrieval_status"], "not_ready");
                assert_eq!(value["status"], "running");
                assert_eq!(value["stdout"], "partial output");
            } else {
                assert_eq!(value["retrieval_status"], "timeout");
                assert_eq!(value["status"], "running");
                assert_eq!(value["stdout"], "partial output");
            }
            assert!(started.elapsed() >= Duration::from_millis(timeout));
        }
        assert!(super::super::tasks::execute_for_session_async(
            "TaskGet",
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
        let request = json!({"task_id":id,"block":true,"timeout":40});
        let value = tokio::time::timeout(
            Duration::from_millis(500),
            control_async("TaskGet", &request, root.path(), &session),
        )
        .await;
        lock.execute_batch("ROLLBACK").unwrap();
        let value = value
            .expect("SQLite busy wait exceeded TaskGet deadline")
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
        let request = json!({"task_id":id,"block":true});
        let (value, _) = tokio::join!(
            control_async("TaskGet", &request, root.path(), &session),
            update
        );
        assert_eq!(value.unwrap().unwrap()["retrieval_status"], "success");
        let id = waiting_fixture(root.path(), &session);
        let request = json!({"task_id":id,"block":true});
        assert!(tokio::time::timeout(
            Duration::from_millis(80),
            control_async("TaskGet", &request, root.path(), &session)
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
            "TaskGet",
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
            "TaskGet",
            &json!({"task_id":id,"block":true}),
            root.path(),
            &session
        )
        .is_err());
    }
    #[tokio::test]
    async fn task_get_reads_gradual_output_and_blocks_until_a_real_shell_completes() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let mut command = Command::new("node");
        command.arg("-e").arg(
            "process.stdout.write('first\\n');console.error('diagnostic\\n');setTimeout(()=>{console.log('second');process.exit(0)},400)",
        );
        let task = scope(Some((root.path().into(), session.clone())), async {
            start(
                command,
                None,
                "node",
                "gradual fixture",
                "node fixture",
                None,
            )
            .unwrap()
        })
        .await;
        let id = task["task_id"].as_str().unwrap();
        // The immediate snapshot shows output produced before completion.
        let mut saw_first_while_running = false;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            let value = super::super::tasks::execute_for_session(
                "TaskGet",
                json!({"task_id":id,"block":false}),
                root.path(),
                &session,
            )
            .unwrap();
            if value["stdout"]
                .as_str()
                .unwrap_or_default()
                .contains("first")
                && value["status"] == "running"
            {
                saw_first_while_running = true;
                assert_eq!(value["retrieval_status"], "not_ready");
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            saw_first_while_running,
            "no running snapshot carried output"
        );
        let value = super::super::tasks::execute_for_session_async(
            "TaskGet",
            json!({"task_id":id,"block":true,"timeout":5_000}),
            root.path(),
            &session,
        )
        .await
        .unwrap();
        assert_eq!(value["status"], "completed");
        assert_eq!(value["retrieval_status"], "success");
        let stdout = value["stdout"].as_str().unwrap();
        assert!(
            stdout.contains("first") && stdout.contains("second"),
            "{stdout}"
        );
        assert!(value["stderr"].as_str().unwrap().contains("diagnostic"));
    }

    #[tokio::test]
    async fn task_get_timeout_leaves_a_real_shell_running() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let mut command = Command::new("node");
        command
            .arg("-e")
            .arg("console.log('ready');setInterval(()=>{},10000)");
        let task = scope(Some((root.path().into(), session.clone())), async {
            start(command, None, "node", "live fixture", "node fixture", None).unwrap()
        })
        .await;
        let id = task["task_id"].as_str().unwrap();
        let directory = root.path().join("shell-tasks").join(&session).join(id);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while tokio::time::Instant::now() < deadline {
            let value = super::super::tasks::execute_for_session(
                "TaskGet",
                json!({"task_id":id,"block":false}),
                root.path(),
                &session,
            )
            .unwrap();
            if value["stdout"]
                .as_str()
                .unwrap_or_default()
                .contains("ready")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let started = tokio::time::Instant::now();
        let value = super::super::tasks::execute_for_session_async(
            "TaskGet",
            json!({"task_id":id,"block":true,"timeout":120}),
            root.path(),
            &session,
        )
        .await
        .unwrap();
        assert!(started.elapsed() >= Duration::from_millis(120));
        assert_eq!(value["retrieval_status"], "timeout");
        assert_eq!(value["status"], "running");
        assert!(value["stdout"].as_str().unwrap().contains("ready"));
        assert!(!directory.join("cancel").exists());
        let stopped = super::super::tasks::execute_for_session(
            "TaskStop",
            json!({"task_id":id}),
            root.path(),
            &session,
        )
        .unwrap();
        assert_eq!(stopped["cancellation_requested"], true);
        let cancelled = super::super::tasks::execute_for_session_async(
            "TaskGet",
            json!({"task_id":id,"block":true,"timeout":5_000}),
            root.path(),
            &session,
        )
        .await
        .unwrap();
        assert_eq!(cancelled["status"], "cancelled");
        assert_eq!(cancelled["retrieval_status"], "success");
    }

    async fn output(root: &Path, session: &str, id: &str, terminal: bool) -> Value {
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let value = super::super::tasks::execute_for_session(
                    "TaskGet",
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
        let done = scope(
            Some((root.path().into(), session.clone())),
            run_foreground(
                quick,
                Duration::from_secs(30),
                "node",
                "quick",
                "node quick",
                None,
            ),
        )
        .await
        .unwrap();
        assert!(done["stdout"].as_str().unwrap().contains("quick"));
        let mut command = Command::new("node");
        command.arg("-e").arg(script);
        let (root_path, id) = (root.path().to_owned(), session.clone());
        let moved = scope(Some((root.path().into(), session.clone())), async move {
            let request = async {
                tokio::time::sleep(Duration::from_millis(400)).await;
                request_background(&root_path, &id).unwrap();
            };
            let (result, ()) = tokio::join!(
                run_foreground(
                    command,
                    Duration::from_secs(30),
                    "node",
                    "slow",
                    "node slow",
                    None
                ),
                request
            );
            result
        })
        .await
        .unwrap();
        assert_eq!(moved["backgrounded"], true);
        let id = moved["task_id"].as_str().unwrap();
        let finished = output(root.path(), &session, id, true).await;
        assert_eq!(finished["status"], "completed");
        let stdout = finished["stdout"].as_str().unwrap();
        assert!(
            stdout.contains("early") && stdout.contains("late"),
            "{stdout}"
        );
        // A request made before a call starts does not move it.
        let mut later = Command::new("node");
        later
            .arg("-e")
            .arg("setTimeout(()=>console.log('kept'),300)");
        let kept = scope(
            Some((root.path().into(), session.clone())),
            run_foreground(
                later,
                Duration::from_secs(30),
                "node",
                "later",
                "node later",
                None,
            ),
        )
        .await
        .unwrap();
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
            start(
                command,
                None,
                "node",
                "test managed shell",
                "node fixture",
                None,
            )
            .unwrap()
        })
        .await;
        let id = task["task_id"].as_str().unwrap();
        let running = output(root.path(), &session, id, false).await;
        assert_eq!(running["status"], "running");
        assert!(running["stderr"].as_str().unwrap().contains("diagnostic"));
        for change in [
            json!({"status":"completed"}),
            json!({"status":"deleted"}),
            json!({"output":"fabricated"}),
            json!({"metadata":{"managed_shell":false}}),
            json!({"metadata":{"managed_shell":null}}),
        ] {
            let mut change = change;
            change["id"] = json!(id);
            assert!(super::super::tasks::execute_for_session(
                "TaskUpdate",
                change,
                root.path(),
                &session
            )
            .is_err());
        }
        super::super::tasks::execute_for_session(
            "TaskUpdate",
            json!({"id":id,"subject":"renamed server","metadata":{"note":"safe user annotation"}}),
            root.path(),
            &session,
        )
        .unwrap();
        let still_running = output(root.path(), &session, id, false).await;
        assert_eq!(still_running["status"], "running");
        assert!(super::super::tasks::execute_for_session(
            "TaskCreate",
            json!({"subject":"fake managed task","metadata":{"managed_shell":true}}),
            root.path(),
            &session
        )
        .is_err());
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
        let cancelled = output(root.path(), &session, id, true).await;
        assert_eq!(cancelled["status"], "cancelled");
        assert_eq!(cancelled["retrieval_status"], "success");
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
                start(
                    command,
                    Some(Duration::from_millis(600)),
                    "node",
                    "fixture",
                    "node fixture",
                    None,
                )
                .unwrap()
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
            assert_eq!(observed["retrieval_status"], "success");
            assert!(!observed["stdout"].as_str().unwrap().is_empty());
            if expected == "failed" {
                assert!(observed["result"].as_str().unwrap().contains("timed_out"));
            }
        }
    }
    #[tokio::test]
    async fn zero_timeout_returns_a_finished_shell_snapshot() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let mut command = Command::new("node");
        command.args(["-e", "console.log('zero-timeout-output')"]);
        let task = scope(Some((root.path().into(), session.clone())), async {
            start(
                command,
                Some(Duration::from_secs(30)),
                "node",
                "fixture",
                "node fixture",
                None,
            )
            .unwrap()
        })
        .await;
        let id = task["task_id"].as_str().unwrap().to_owned();
        let finished = output(root.path(), &session, &id, true).await;
        assert_eq!(finished["status"], "completed");
        let value = super::super::tasks::execute_for_session_async(
            "TaskGet",
            json!({"task_id":id,"block":true,"timeout":0}),
            root.path(),
            &session,
        )
        .await
        .unwrap();
        assert_eq!(value["status"], "completed", "{value}");
        assert!(
            value["stdout"]
                .as_str()
                .unwrap_or_default()
                .contains("zero-timeout-output"),
            "{value}"
        );
    }
    #[tokio::test]
    async fn legacy_task_output_waits_by_default() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let mut command = Command::new("node");
        command.args(["-e", "setTimeout(()=>console.log('legacy-late-output'),300)"]);
        let task = scope(Some((root.path().into(), session.clone())), async {
            start(
                command,
                Some(Duration::from_secs(30)),
                "node",
                "fixture",
                "node fixture",
                None,
            )
            .unwrap()
        })
        .await;
        let id = task["task_id"].as_str().unwrap().to_owned();
        let value = super::super::tasks::execute_for_session_async(
            "TaskOutput",
            json!({"task_id":id}),
            root.path(),
            &session,
        )
        .await
        .unwrap();
        assert_eq!(value["status"], "completed", "{value}");
        assert_eq!(value["retrieval_status"], "success", "{value}");
        assert!(
            value["stdout"]
                .as_str()
                .unwrap_or_default()
                .contains("legacy-late-output"),
            "{value}"
        );
    }
}

#[cfg(test)]
mod foreground_output_tests {
    use super::*;
    #[test]
    fn output_tracks_appends_and_excludes_other_sessions_and_background_tasks() {
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let path = root
            .path()
            .join("shell-tasks")
            .join(&session)
            .join("fg-test");
        std::fs::create_dir_all(&path).unwrap();
        std::fs::write(path.join("foreground-active"), b"running").unwrap();
        std::fs::write(path.join("stdout.log"), b"first").unwrap();
        assert!(foreground_output(root.path(), &session)
            .unwrap()
            .contains("first"));
        std::fs::write(path.join("stdout.log"), b"first\nsecond").unwrap();
        assert!(foreground_output(root.path(), &session)
            .unwrap()
            .contains("second"));
        assert!(
            foreground_output(root.path(), &uuid::Uuid::new_v4().to_string())
                .unwrap()
                .is_empty()
        );
        std::fs::remove_file(path.join("foreground-active")).unwrap();
        assert!(foreground_output(root.path(), &session).unwrap().is_empty());
        assert!(foreground_output(root.path(), "../invalid").is_err());
    }
}
