//! Native stdin input for an interactive managed shell.
//!
//! A shell started with `Bash interaction:"pipe"` reads its stdin from a FIFO in
//! its task directory, so any later CLI process can write to the same process
//! without sharing memory with the owner. The owner keeps one write side of the
//! FIFO open; transient writers can therefore come and go without the shell
//! seeing end-of-input. An explicit EOF marker makes the owner drop that keepalive
//! handle, which is what the shell observes as EOF.
//!
//! Reported `bytes_written` is what the operating system accepted into the pipe,
//! not what the process consumed. A write that cannot make progress before its
//! deadline reports the bytes already accepted and is never retried automatically.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

/// Bytes one call may write. Smaller than typical demand so a call stays bounded.
pub(crate) const MAX_INPUT_BYTES: usize = 64 * 1024;
/// Default bound on how long a call waits for pipe capacity and the task lock.
pub(crate) const DEFAULT_WRITE_MS: u64 = 10_000;
/// Hard ceiling for one call's wait.
pub(crate) const MAX_WRITE_MS: u64 = 60_000;

pub(super) fn definition() -> super::ToolDefinition {
    super::ToolDefinition {
        name: "TaskInput".into(),
        description: "Write UTF-8 data to the stdin of an interactive background shell started with Bash `interaction:\"pipe\"`. data is written first, then eof:true closes stdin. Each call waits at most timeout milliseconds for pipe capacity (default 10000, maximum 60000) and reports bytes_written as the bytes the operating system accepted, which is not a claim that the process consumed them; a timed-out write returns partial progress and is never retried for you. Repeated eof is harmless, and writing after EOF or to a task that is not running fails without changing the task. This is a mutating operation: it needs the owning session, normal approval and hooks, and it is refused in plan mode. Read output with TaskGet and stop the process with TaskStop.".into(),
        input_schema: json!({
            "type":"object",
            "properties":{
                "task_id":{"type":"string","description":"Task ID of the interactive pipe shell."},
                "taskId":{"type":"string","description":"Alias for task_id."},
                "id":{"type":"string","description":"Alias for task_id."},
                "data":{"type":"string","description":format!("UTF-8 text to write. At most {MAX_INPUT_BYTES} bytes per call.")},
                "eof":{"type":"boolean","default":false,"description":"Close stdin after writing data (or immediately when data is omitted)."},
                "timeout":{"type":"integer","minimum":1,"maximum":60000,"default":10000,"description":"Wait limit in milliseconds for the task lock and pipe capacity."},
            },
            "required":[],
            "additionalProperties":false
        }),
    }
}

pub(crate) fn is_input(name: &str) -> bool {
    name == "TaskInput"
}

/// Write to (or close) one interactive shell's stdin in the caller's session.
pub(crate) fn execute(input: &Value, root: &Path, session: &str) -> Result<Value> {
    execute_cancellable(input, root, session, None)
}

/// The flag is set when the caller drops the pending call (a cancelled tool
/// execution). The blocking writer checks it while waiting, so a cancelled call
/// stops acquiring capacity instead of writing to stdin after the turn ended.
pub(crate) fn execute_cancellable(
    input: &Value,
    root: &Path,
    session: &str,
    cancelled: Option<&AtomicBool>,
) -> Result<Value> {
    let task_id = input["task_id"]
        .as_str()
        .or_else(|| input["taskId"].as_str())
        .or_else(|| input["id"].as_str())
        .filter(|id| !id.trim().is_empty())
        .context("task_id is required")?
        .to_owned();
    let data = match input.get("data") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text.clone()),
        Some(_) => bail!("TaskInput data must be a string"),
    };
    let eof = match input.get("eof") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(_) => bail!("TaskInput eof must be a boolean"),
    };
    ensure!(
        data.is_some() || eof,
        "TaskInput needs data and/or eof: true"
    );
    if let Some(data) = &data {
        ensure!(
            !data.is_empty() || eof,
            "TaskInput data is empty and eof is false, so there is nothing to write"
        );
        ensure!(
            data.len() <= MAX_INPUT_BYTES,
            "TaskInput data is {} bytes; one call accepts at most {MAX_INPUT_BYTES} bytes",
            data.len()
        );
    }
    let timeout = Duration::from_millis(match input.get("timeout").filter(|value| !value.is_null()) {
        Some(value) => value
            .as_u64()
            .filter(|ms| (1..=MAX_WRITE_MS).contains(ms))
            .with_context(|| {
                format!("TaskInput timeout must be an integer number of milliseconds in 1..={MAX_WRITE_MS}")
            })?,
        None => DEFAULT_WRITE_MS,
    });

    // Ownership is the caller's own session database: an ID alone is never an
    // authorization, and another session simply has no such task record.
    let task = super::tasks::read_for_session(&task_id, root, session)?;
    ensure!(
        task["metadata"]["managed_shell"] == true,
        "Task {task_id} is not a managed shell task; TaskInput only writes to one started with Bash interaction:\"pipe\""
    );
    ensure!(
        task["metadata"]["interaction"].as_str() == Some("pipe"),
        "Task {task_id} has no stdin pipe; start it with Bash interaction:\"pipe\" and run_in_background:true"
    );
    let stored = task["status"].as_str().unwrap_or("unknown");
    ensure!(
        stored == "running",
        "Task {task_id} is not running (status {stored}); stdin is unavailable{}",
        if stored == "completed" || stored == "failed" || stored == "cancelled" {
            ". Start a new interactive shell if you need one"
        } else {
            ""
        }
    );
    let directory = super::background_shell::task_directory(root, session, &task)?;
    // A lease that is missing, free, or only a legacy PID proves nothing about
    // a currently writable channel. Refuse instead of treating an unverifiable
    // identity as permission to write.
    let owner = super::background_shell::OwnerProbe::open(&directory)?;
    let state = owner.state();
    ensure!(state.is_held(), "{}", state.refusal(&task_id));
    let status = super::background_shell::task_status(root, session, &task)?;
    ensure!(
        status == "running",
        "Task {task_id} is running but its owner stopped reporting a heartbeat (status {status}); stdin is unavailable. Use TaskStop to cancel it"
    );
    write_stdin(
        &task_id,
        &directory,
        &owner,
        data.as_deref(),
        eof,
        timeout,
        cancelled,
    )
}

/// Test-only one-shot latches for the two places a write can wait.
///
/// Thread-local, so each latch belongs to the one call that installed it: tests
/// running in parallel cannot see each other's latch, and there is no global or
/// environment hook. Compiled out of release builds and neither latch changes
/// what the write does.
#[cfg(test)]
pub(crate) mod capacity_barrier {
    use std::cell::RefCell;

    type Hook = Box<dyn FnMut(&str) + Send>;

    thread_local! {
        static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
        static LOCK_HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
    }

    pub(crate) fn install(hook: Hook) {
        HOOK.with(|slot| *slot.borrow_mut() = Some(hook));
    }

    pub(crate) fn clear() {
        HOOK.with(|slot| *slot.borrow_mut() = None);
    }

    /// Called from the real `WouldBlock` arm, so a test observes the exact
    /// moment the production call has filled the pipe and must wait for room.
    pub(super) fn reached(task_id: &str) {
        HOOK.with(|slot| {
            if let Some(hook) = slot.borrow_mut().as_mut() {
                hook(task_id);
            }
        });
    }

    /// Latch for the per-task `stdin.lock` wait. It fires from the real
    /// `WouldBlock` arm, which is only reached after this iteration's live-owner
    /// validation passed, so it proves the call is past the initial owner checks
    /// and actually waiting for the lock.
    pub(crate) fn install_lock_wait(hook: Hook) {
        LOCK_HOOK.with(|slot| *slot.borrow_mut() = Some(hook));
    }

    pub(crate) fn clear_lock_wait() {
        LOCK_HOOK.with(|slot| *slot.borrow_mut() = None);
    }

    pub(super) fn lock_wait_reached(task_id: &str) {
        LOCK_HOOK.with(|slot| {
            if let Some(hook) = slot.borrow_mut().as_mut() {
                hook(task_id);
            }
        });
    }
}

fn is_cancelled(flag: Option<&AtomicBool>) -> bool {
    flag.is_some_and(|flag| flag.load(Ordering::Acquire))
}

/// The owner is no longer established. Reported like any other partial write:
/// whatever the OS already accepted stays counted and is never retried.
fn lost_owner_result(
    task_id: &str,
    state: &super::background_shell::OwnerState,
    written: usize,
    remaining: usize,
    eof: bool,
) -> Value {
    partial_result(
        task_id,
        written,
        remaining,
        eof,
        &format!(
            "TaskInput stopped after {written} of {} bytes: {}",
            written + remaining,
            state.refusal(task_id)
        ),
        json!({"owner_lost": true}),
    )
}

/// A failed or cancelled write keeps the byte count: the caller must be able to
/// tell what the operating system already accepted.
fn partial_result(
    task_id: &str,
    written: usize,
    remaining: usize,
    eof: bool,
    reason: &str,
    extra: Value,
) -> Value {
    let mut value = json!({
        "task_id": task_id,
        "input_mode": "pipe",
        "bytes_written": written,
        "bytes_remaining": remaining,
        "eof": false,
        "stdin_closed": false,
        "write_complete": false,
        "timed_out": false,
        "is_error": true,
        "error": reason,
        "message": format!(
            "TaskInput wrote {written} of {} bytes and did not finish; some bytes may already have reached the process, so this call must not be repeated blindly.",
            written + remaining
        ),
        "requested_eof": eof,
    });
    if let (Some(value), Some(extra)) = (value.as_object_mut(), extra.as_object()) {
        value.extend(extra.clone());
    }
    value
}

#[cfg(unix)]
fn write_stdin(
    task_id: &str,
    directory: &Path,
    owner: &super::background_shell::OwnerProbe,
    data: Option<&str>,
    eof: bool,
    timeout: Duration,
    cancelled: Option<&AtomicBool>,
) -> Result<Value> {
    use std::io::Write;
    if is_cancelled(cancelled) {
        return Ok(partial_result(
            task_id,
            0,
            data.map_or(0, str::len),
            eof,
            "The call was cancelled before it touched stdin",
            json!({"cancelled": true}),
        ));
    }
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join("stdin.lock"))
        .context("Opening the task stdin lock")?;
    let deadline = Instant::now() + timeout;
    // Writes and EOF are serialized per task, including across processes. The
    // cancellation flag is checked here as well: a cancelled call must not
    // acquire the lock later and write after the turn is gone.
    loop {
        if is_cancelled(cancelled) {
            return Ok(partial_result(
                task_id,
                0,
                data.map_or(0, str::len),
                eof,
                "The call was cancelled while waiting for the task stdin lock",
                json!({"cancelled": true}),
            ));
        }
        // Waiting for the lock must not outlive the owner: a dead owner can
        // never consume what this call would write.
        let state = owner.state();
        if !state.is_held() {
            return Ok(lost_owner_result(
                task_id,
                &state,
                0,
                data.map_or(0, str::len),
                eof,
            ));
        }
        match crate::fs::try_lock_file(&lock) {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) => {
                // Reached only after the live-owner check above, so a test that
                // observes this marker knows the call is genuinely in the lock
                // wait, not still in the initial owner validation.
                #[cfg(test)]
                capacity_barrier::lock_wait_reached(task_id);
                ensure!(
                    Instant::now() < deadline,
                    "Another TaskInput call still holds task {task_id} stdin; try again"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(error).context("Locking the task stdin")
            }
        }
    }
    // The owner may have died while this call waited for the lock. Only a held
    // lease proves the channel still has its real owner.
    let state = owner.state();
    if !state.is_held() {
        return Ok(lost_owner_result(
            task_id,
            &state,
            0,
            data.map_or(0, str::len),
            eof,
        ));
    }
    let eof_marker = directory.join("stdin-eof");
    if eof_marker.exists() {
        ensure!(
            data.is_none(),
            "Task {task_id} stdin is closed: EOF was already requested"
        );
        return Ok(json!({
            "task_id": task_id,
            "input_mode": "pipe",
            "bytes_written": 0,
            "eof": true,
            "stdin_closed": true,
            "already_closed": true,
            "write_complete": true,
            "timed_out": false,
            "message": "stdin was already closed; EOF is idempotent"
        }));
    }
    let mut written = 0usize;
    if let Some(data) = data {
        if is_cancelled(cancelled) {
            return Ok(partial_result(
                task_id,
                0,
                data.len(),
                eof,
                "The call was cancelled before it opened the stdin pipe",
                json!({"cancelled": true}),
            ));
        }
        let fifo = directory.join("stdin.pipe");
        // `CLOEXEC` is required: this transient write side is opened while the
        // call may block for a long time, and any shell or helper process
        // started meanwhile would otherwise inherit a copy that keeps the shell's
        // stdin pipe open after the owner closes its own, so the shell would
        // never see EOF.
        let fd = match rustix::fs::open(
            &fifo,
            rustix::fs::OFlags::WRONLY
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(error) => {
                return Ok(partial_result(
                    task_id,
                    0,
                    data.len(),
                    eof,
                    &format!(
                        "Task {task_id} stdin pipe is not available ({error}); the process has exited or is no longer reading it"
                    ),
                    json!({"stdin_available": false}),
                ))
            }
        };
        let mut file = std::fs::File::from(fd);
        let mut remaining = data.as_bytes();
        while !remaining.is_empty() {
            if is_cancelled(cancelled) {
                return Ok(partial_result(
                    task_id,
                    written,
                    remaining.len(),
                    eof,
                    "The call was cancelled; it stopped writing and did not retry",
                    json!({"cancelled": true}),
                ));
            }
            // A blocked write must not keep feeding a channel whose owner died
            // while the call was waiting for room.
            let state = owner.state();
            if !state.is_held() {
                return Ok(lost_owner_result(
                    task_id,
                    &state,
                    written,
                    remaining.len(),
                    eof,
                ));
            }
            match file.write(remaining) {
                Ok(0) => {
                    return Ok(partial_result(
                        task_id,
                        written,
                        remaining.len(),
                        eof,
                        "the stdin pipe accepted no bytes",
                        json!({}),
                    ))
                }
                Ok(count) => {
                    written += count;
                    remaining = &remaining[count..];
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    // The pipe is full: this is the capacity wait a test can
                    // observe exactly, rather than infer from elapsed time.
                    #[cfg(test)]
                    capacity_barrier::reached(task_id);
                    if Instant::now() >= deadline {
                        // Report exactly what the OS accepted. The bytes already
                        // written cannot be recalled and are never retried here.
                        return Ok(json!({
                            "task_id": task_id,
                            "input_mode": "pipe",
                            "bytes_written": written,
                            "bytes_remaining": remaining.len(),
                            "eof": false,
                            "stdin_closed": false,
                            "write_complete": false,
                            "timed_out": true,
                            "is_error": true,
                            "error": format!(
                                "TaskInput wrote {written} of {} bytes; the pipe stayed full for {} ms and the process is still running. Do not repeat this call blindly: some bytes were already accepted.",
                                written + remaining.len(),
                                timeout.as_millis()
                            )
                        }));
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    // A broken pipe after a partial write keeps the byte count.
                    return Ok(partial_result(
                        task_id,
                        written,
                        remaining.len(),
                        eof,
                        &format!("Writing to the task stdin pipe failed: {error}"),
                        json!({"stdin_available": false}),
                    ));
                }
            }
        }
    }
    if is_cancelled(cancelled) {
        return Ok(partial_result(
            task_id,
            written,
            0,
            eof,
            "The call was cancelled before stdin EOF was requested; no EOF marker was written",
            json!({"cancelled": true}),
        ));
    }
    if eof {
        // EOF is the one write that cannot be expressed by the byte count, so
        // the owner is checked once more right before it is recorded.
        let state = owner.state();
        if !state.is_held() {
            return Ok(lost_owner_result(task_id, &state, written, 0, eof));
        }
        std::fs::write(&eof_marker, b"eof").context("Requesting task stdin EOF")?;
    }
    Ok(json!({
        "task_id": task_id,
        "input_mode": "pipe",
        "bytes_written": written,
        "eof": eof,
        "stdin_closed": eof,
        "already_closed": false,
        "write_complete": true,
        "timed_out": false,
        "message": if eof {
            "Data was written and stdin is closing; the process observes EOF after the buffered bytes."
        } else {
            "Data was written to the process stdin pipe."
        }
    }))
}

#[cfg(not(unix))]
fn write_stdin(
    _task_id: &str,
    _directory: &Path,
    _owner: &super::background_shell::OwnerProbe,
    _data: Option<&str>,
    _eof: bool,
    _timeout: Duration,
    _cancelled: Option<&AtomicBool>,
) -> Result<Value> {
    bail!("TaskInput needs a POSIX stdin pipe; interactive input is not implemented on this platform yet (named pipes arrive with the PTY increment)")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A lease that exists but is not held proves the owner is gone, so a call
    /// that reaches the write path after the lock must not write anything.
    #[cfg(unix)]
    #[test]
    fn a_released_owner_lease_stops_the_write_after_the_lock_is_taken() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path();
        std::fs::write(directory.join("owner.lock"), b"").unwrap();
        let owner = super::super::background_shell::OwnerProbe::open(directory).unwrap();
        assert!(matches!(
            owner.state(),
            super::super::background_shell::OwnerState::Gone(_)
        ));
        let result = write_stdin(
            "fixture-task",
            directory,
            &owner,
            Some("must-not-be-written"),
            false,
            Duration::from_millis(100),
            None,
        )
        .unwrap();
        assert_eq!(result["owner_lost"], true, "{result}");
        assert_eq!(result["bytes_written"], 0, "{result}");
        assert_eq!(result["write_complete"], false, "{result}");
        assert_eq!(result["is_error"], true, "{result}");
        assert!(
            result["error"]
                .as_str()
                .unwrap_or_default()
                .contains("owner lease is released"),
            "{result}"
        );
    }

    #[test]
    fn task_input_is_a_discoverable_non_core_builtin() {
        let names: Vec<String> = super::super::get_tool_definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect();
        assert!(names.contains(&"TaskInput".to_string()), "{names:?}");
        // The folded legacy reader stays unadvertised.
        assert!(!names.contains(&"TaskOutput".to_string()));
        assert!(super::super::tasks::is_task("TaskInput"));
        assert!(!crate::services::customization::is_core_tool("TaskInput"));
        assert_eq!(
            crate::services::customization::default_loading("TaskInput"),
            crate::services::customization::ToolLoading {
                enabled: false,
                on_demand: true
            }
        );
        let definition = definition();
        assert_eq!(
            definition.input_schema["properties"]["eof"]["type"],
            "boolean"
        );
        assert_eq!(
            definition.input_schema["properties"]["data"]["type"],
            "string"
        );
    }

    #[test]
    fn validation_is_explicit_before_any_task_lookup() {
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let missing = json!({"task_id":"x"});
        let error = execute(&missing, root.path(), &session).unwrap_err();
        assert!(format!("{error}").contains("data and/or eof"), "{error}");
        let bad_data = json!({"task_id":"x","data":7});
        let error = execute(&bad_data, root.path(), &session).unwrap_err();
        assert!(format!("{error}").contains("must be a string"), "{error}");
        let too_big = json!({"task_id":"x","data":"a".repeat(MAX_INPUT_BYTES + 1)});
        let error = execute(&too_big, root.path(), &session).unwrap_err();
        assert!(format!("{error}").contains("at most"), "{error}");
        let bad_timeout = json!({"task_id":"x","data":"a","timeout":0});
        let error = execute(&bad_timeout, root.path(), &session).unwrap_err();
        assert!(format!("{error}").contains("timeout"), "{error}");
        // A task that does not exist in this session is never written to.
        let unknown = json!({"task_id":uuid::Uuid::new_v4().to_string(),"data":"a"});
        let error = execute(&unknown, root.path(), &session).unwrap_err();
        assert!(
            format!("{error}").contains("Task not found")
                || format!("{error}").contains("not a managed shell"),
            "{error}"
        );
    }
}
