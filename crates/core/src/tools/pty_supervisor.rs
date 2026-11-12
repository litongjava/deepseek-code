//! Independent supervisor for the managed Linux PTY owner.
//!
//! The owner is a thread inside a multi-threaded Rust/Tokio host. `Drop` and
//! `OwnerGuard` cannot run when that host is killed, so the terminal session is
//! guarded by a separate process that is started **before** the PTY child and
//! outlives the owner:
//!
//! * it is the same host executable, execed with a private environment
//!   variable (the pattern used by `services::background_sessions`), so it
//!   inherits no locks, allocator state, Tokio runtime or descriptors;
//! * `pre_exec` performs exactly one async-signal-safe call, `setsid()`, before
//!   the immediate `exec`. It is deliberately **not** combined with
//!   `process_group(0)`: std runs `setpgid(0, 0)` before `pre_exec`, and
//!   `setsid()` on an existing process-group leader fails with `EPERM`;
//! * it holds a pidfd for the owner (verified PID **and** start tick) and polls
//!   it together with the file protocol.
//!
//! The startup handshake removes the unowned window:
//!
//! 1. the supervisor writes `supervisor.ready` before the owner creates a child;
//! 2. the owner spawns the *execution gate* (see [`super::pty_gate`]), which
//!    cannot run the user program until it receives `GO`;
//! 3. the owner publishes `session.json` and waits for `session.ack`;
//! 4. only then is `GO` written.
//!
//! If the owner dies at any point, either the gate exits on pipe EOF without
//! running the user program, or the supervisor knows the exact published
//! session and cleans it. Nothing is guessed from `/proc` after the fact.
//!
//! Session cleanup reuses [`super::pty_transport::cleanup_owned_session`],
//! which signals each member through a pidfd after re-reading its session and
//! start tick. A PID alone never authorizes a signal.
#![cfg(target_os = "linux")]

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::{
    io::Write,
    os::unix::{fs::OpenOptionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use super::pty_transport;

pub(crate) const DIR_ENV: &str = "DEEPSEEK_CODE_PTY_SUPERVISOR_DIR";
pub(crate) const OWNER_PID_ENV: &str = "DEEPSEEK_CODE_PTY_SUPERVISOR_OWNER_PID";
pub(crate) const OWNER_START_ENV: &str = "DEEPSEEK_CODE_PTY_SUPERVISOR_OWNER_START";

const READY_FILE: &str = "supervisor.ready";
const SESSION_FILE: &str = "session.json";
const ACK_FILE: &str = "session.ack";
const DONE_FILE: &str = "supervisor.done";
const FORCE_FILE: &str = "supervisor.force-clean";
const STATE_FILE: &str = "supervisor-state.json";

const READY_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_SLICE: Duration = Duration::from_millis(50);
const CLEANUP_BOUND: Duration = Duration::from_secs(10);

fn write_atomic(path: &Path, value: &Value) -> Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let name = format!(
        ".{}.new-{}-{}-{}",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("state"),
        std::process::id(),
        chrono::Utc::now().timestamp_millis(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let staging = path.with_file_name(name);
    let result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&staging)
            .with_context(|| format!("Creating {}", staging.display()))?;
        file.write_all(value.to_string().as_bytes())
            .with_context(|| format!("Writing {}", staging.display()))?;
        file.flush()
            .with_context(|| format!("Flushing {}", staging.display()))?;
        std::fs::rename(&staging, path)
            .with_context(|| format!("Publishing {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&staging);
    }
    result
}

fn read_json(path: &Path) -> Option<Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
}

pub(crate) struct SessionPublication {
    pub session: u32,
    pub leader_start: u64,
}

fn read_session_published(directory: &Path) -> Option<SessionPublication> {
    let value = read_json(&directory.join(SESSION_FILE))?;
    Some(SessionPublication {
        session: value
            .get("session")
            .and_then(Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())?,
        leader_start: value.get("leader_start_tick").and_then(Value::as_u64)?,
    })
}

fn owner_exited(pidfd: &rustix::fd::OwnedFd) -> bool {
    let mut fds = [rustix::event::PollFd::new(
        pidfd,
        rustix::event::PollFlags::IN,
    )];
    let timeout = rustix::event::Timespec::try_from(POLL_SLICE).unwrap_or_default();
    match rustix::event::poll(&mut fds, Some(&timeout)) {
        Ok(0) => false,
        Ok(_) => fds[0].revents().contains(rustix::event::PollFlags::IN),
        // A poll failure is not proof of survival: report it as exit so the
        // caller takes the conservative cleanup path.
        Err(_) => true,
    }
}

/// Host entry: run the supervisor when this process was started for it.
///
/// Returns `Ok(false)` for a normal host start.
pub(crate) fn run_from_env() -> Result<bool> {
    let Some(directory) = std::env::var_os(DIR_ENV) else {
        return Ok(false);
    };
    std::env::remove_var(DIR_ENV);
    let owner_pid: u32 = std::env::var(OWNER_PID_ENV)
        .context("The managed PTY supervisor has no owner PID")?
        .parse()
        .context("The managed PTY supervisor owner PID is not a number")?;
    let owner_start: u64 = std::env::var(OWNER_START_ENV)
        .context("The managed PTY supervisor has no owner start tick")?
        .parse()
        .context("The managed PTY supervisor owner start tick is not a number")?;
    std::env::remove_var(OWNER_PID_ENV);
    std::env::remove_var(OWNER_START_ENV);
    supervise(Path::new(&directory), owner_pid, owner_start)
}

fn supervise(directory: &Path, owner_pid: u32, owner_start: u64) -> Result<bool> {
    let pidfd = match rustix::process::Pid::from_raw(owner_pid as i32)
        .map(|pid| rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()))
    {
        Some(Ok(pidfd)) => pidfd,
        Some(Err(_)) | None => {
            // The owner is already gone before it could wait for readiness.
            let _ = write_atomic(
                &directory.join(STATE_FILE),
                &json!({
                    "version":1,
                    "status":"failed",
                    "owner_lost":true,
                    "owner_absent_at_start":true,
                    "session_published":false,
                    "os_cleanup_complete":false,
                    "output_finalized":false,
                    "task_record_updated":false,
                    "persistence_complete":true,
                    "error":"The owner process was already gone when its supervisor started; no session identity exists."
                }),
            );
            return Ok(true);
        }
    };
    match pty_transport::process_start_tick(owner_pid) {
        Ok(Some(tick)) if tick == owner_start => {}
        Ok(Some(_)) => {
            let _ = write_atomic(
                &directory.join(STATE_FILE),
                &json!({
                    "version":1,
                    "status":"failed",
                    "owner_lost":false,
                    "identity_mismatch":true,
                    "session_published":false,
                    "os_cleanup_complete":false,
                    "output_finalized":false,
                    "task_record_updated":false,
                    "persistence_complete":true,
                    "error":format!(
                        "PID {owner_pid} now belongs to a different process instance; refusing to supervise it or to signal anything on its behalf"
                    )
                }),
            );
            return Ok(true);
        }
        Ok(None) | Err(_) => {
            let _ = write_atomic(
                &directory.join(STATE_FILE),
                &json!({
                    "version":1,
                    "status":"failed",
                    "owner_lost":true,
                    "owner_absent_at_start":true,
                    "session_published":false,
                    "os_cleanup_complete":false,
                    "output_finalized":false,
                    "task_record_updated":false,
                    "persistence_complete":true,
                    "error":"The owner identity could not be verified before supervision started; nothing was signalled."
                }),
            );
            return Ok(true);
        }
    }

    write_atomic(
        &directory.join(READY_FILE),
        &json!({
            "version":1,
            "supervisor_pid":std::process::id(),
            "owner_pid":owner_pid,
            "owner_start_tick":owner_start,
            "ready_ms":chrono::Utc::now().timestamp_millis()
        }),
    )?;

    let mut published = None;
    let mut force_cleanup = false;
    loop {
        if directory.join(FORCE_FILE).exists() {
            force_cleanup = true;
            break;
        }
        if directory.join(DONE_FILE).exists() {
            break;
        }
        if published.is_none() {
            if let Some(publication) = read_session_published(directory) {
                let observation = pty_transport::process_start_tick(publication.session);
                let accepted = match &observation {
                    Ok(Some(tick)) => *tick == publication.leader_start,
                    // The leader may have exited already; a missing process is
                    // not an identity conflict.
                    Ok(None) => true,
                    Err(_) => false,
                };
                let ack = json!({
                    "version":1,
                    "ok":accepted,
                    "session":publication.session,
                    "leader_start_tick":publication.leader_start,
                    "leader_observed":matches!(&observation, Ok(Some(_))),
                    "acked_ms":chrono::Utc::now().timestamp_millis(),
                    "error": if accepted { Value::Null } else {
                        json!("The published session leader identity does not match the live process; the owner must not start the user program")
                    }
                });
                write_atomic(&directory.join(ACK_FILE), &ack)?;
                if accepted {
                    published = Some(publication);
                }
            }
        }
        if owner_exited(&pidfd) {
            break;
        }
    }

    let owner_finalized = directory.join("terminal-state.json").exists();
    if owner_finalized && !force_cleanup {
        let recorded = read_json(&directory.join("terminal-state.json"));
        write_atomic(
            &directory.join(STATE_FILE),
            &finalized_state(recorded.as_ref(), published.is_some()),
        )?;
        return Ok(true);
    }

    let state = match published {
        Some(publication) => {
            let report = pty_transport::cleanup_owned_session(
                publication.session,
                (publication.session, publication.leader_start),
                Instant::now() + CLEANUP_BOUND,
            );
            json!({
                "version":1,
                "status":"failed",
                "owner_lost":true,
                "owner_finalized":false,
                "cleanup_performed_by":"supervisor",
                "session_published":true,
                "session":publication.session,
                "leader_pid":publication.session,
                "leader_start_tick":publication.leader_start,
                "os_cleanup_complete":report.complete,
                "cleaned_pids":report.cleaned,
                "remaining_pids":report.remaining,
                "cleanup_error":report.error,
                "output_finalized":false,
                "task_record_updated":false,
                "persistence_complete":true,
                "error": if report.complete { Value::Null } else {
                    json!("The terminal session could not be fully cleaned; see cleanup_error and remaining_pids")
                },
                "recorded_ms":chrono::Utc::now().timestamp_millis()
            })
        }
        None => json!({
            "version":1,
            "status":"failed",
            "owner_lost":true,
            "owner_finalized":false,
            "session_published":false,
            "os_cleanup_complete":false,
            "output_finalized":false,
            "task_record_updated":false,
            "persistence_complete":true,
            "error":"No session identity was published before the owner disappeared. The execution gate exits on pipe EOF and never runs the user program; ownership of any terminal session is unknown, so nothing is guessed."
        }),
    };
    write_atomic(&directory.join(STATE_FILE), &state)?;
    Ok(true)
}

/// The executable and fixed arguments that start this supervisor.
fn supervisor_command() -> Result<PathBuf> {
    #[cfg(test)]
    {
        Ok(std::env::current_exe().context("Locating the test executable")?)
    }
    #[cfg(not(test))]
    {
        Ok(std::env::current_exe()
            .context("Locating the host executable for the PTY supervisor")?)
    }
}

#[cfg(test)]
fn supervisor_arguments() -> Vec<std::ffi::OsString> {
    vec![
        std::ffi::OsString::from("--exact"),
        std::ffi::OsString::from("tools::pty_supervisor::tests::pty_supervisor_process_entry"),
        std::ffi::OsString::from("--quiet"),
    ]
}

#[cfg(not(test))]
fn supervisor_arguments() -> Vec<std::ffi::OsString> {
    Vec::new()
}

/// Live handle for one supervisor process.
pub(crate) struct SupervisorHandle {
    child: Child,
    directory: PathBuf,
    finished: bool,
}

impl SupervisorHandle {
    pub(crate) fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Stop the supervisor after the owner reached its finalizer.
    ///
    /// `normal` means the owner wrote `terminal-state.json`; the supervisor
    /// then records the outcome and exits without touching the session.
    /// `normal = false` asks it to clean the published session itself (the
    /// owner thread panicked or never finalized).
    pub(crate) fn finish(&mut self, normal: bool, deadline: Instant) -> Result<()> {
        if self.finished {
            return Ok(());
        }
        let marker = if normal { DONE_FILE } else { FORCE_FILE };
        std::fs::write(self.directory.join(marker), b"stop")
            .with_context(|| format!("Signalling the PTY supervisor with {marker}"))?;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => {
                    self.finished = true;
                    return Ok(());
                }
                Ok(None) => {}
                Err(error) => return Err(error).context("Waiting for the PTY supervisor"),
            }
            if Instant::now() >= deadline {
                // Never report a stray finalizer as a clean shutdown.
                let _ = self.child.kill();
                let _ = self.child.wait();
                self.finished = true;
                bail!(
                    "the managed PTY supervisor for {} did not exit after {marker}",
                    self.directory.display()
                );
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for SupervisorHandle {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// The record for an owner that reached its own finalizer.
///
/// The supervisor does not own the task lifecycle and cannot verify the durable
/// task record: it reports the terminal status the owner wrote, and an absent or
/// unreadable terminal state is an explicit failure rather than a success. A
/// supervisor that merely exited normally says nothing about the user program.
fn finalized_state(recorded: Option<&Value>, published: bool) -> Value {
    let recorded = recorded.unwrap_or(&Value::Null);
    let owner_status = recorded
        .get("status")
        .and_then(Value::as_str)
        .filter(|status| matches!(*status, "completed" | "failed" | "cancelled"));
    let (status, error) = match owner_status {
        Some(status) => (status, Value::Null),
        None => (
            "failed",
            json!("The owner wrote terminal-state.json but it has no readable terminal status; the supervisor does not report a successful task"),
        ),
    };
    json!({
        "version":1,
        "status":status,
        "owner_lost":false,
        "owner_finalized":true,
        "cleanup_performed_by":"owner",
        "session_published":published,
        "os_cleanup_complete":recorded.get("cleanup_complete").and_then(Value::as_bool).unwrap_or(false),
        "output_finalized":recorded.get("output_capture_complete").and_then(Value::as_bool).unwrap_or(false),
        // The supervisor did not write the task record. `null` means the owner
        // left no statement about it, which is not evidence of success.
        "task_record_updated":recorded.get("task_record_updated").cloned().unwrap_or(Value::Null),
        "task_record_update_known":recorded.get("task_record_updated").and_then(Value::as_bool).is_some(),
        "persistence_complete":true,
        "error":error,
        "recorded_ms":chrono::Utc::now().timestamp_millis()
    })
}

/// Start the supervisor and wait until it is ready to observe the owner.
///
/// Called before the PTY child exists, so a failure here leaves no child to
/// clean.
pub(crate) fn spawn(
    directory: &Path,
    owner_pid: u32,
    deadline: Instant,
) -> Result<SupervisorHandle> {
    let owner_start = pty_transport::process_start_tick(owner_pid)
        .context("Reading the owner process identity")?
        .context("The owner process has no readable identity")?;
    let executable = supervisor_command()?;
    let mut command = Command::new(executable);
    command
        .args(supervisor_arguments())
        .env_clear()
        .env(DIR_ENV, directory)
        .env(OWNER_PID_ENV, owner_pid.to_string())
        .env(OWNER_START_ENV, owner_start.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // One async-signal-safe call between fork and the immediate exec. `setsid`
    // is used alone: std would have called `setpgid(0, 0)` for
    // `process_group(0)`, and `setsid` on a process-group leader fails EPERM.
    unsafe {
        command.pre_exec(|| {
            rustix::process::setsid()
                .map(|_| ())
                .map_err(std::io::Error::from)
        });
    }
    let child = command
        .spawn()
        .context("Starting the managed PTY supervisor")?;
    let mut handle = SupervisorHandle {
        child,
        directory: directory.to_path_buf(),
        finished: false,
    };
    loop {
        if directory.join(READY_FILE).exists() {
            return Ok(handle);
        }
        match handle.child.try_wait() {
            Ok(Some(status)) => {
                handle.finished = true;
                bail!(
                    "the managed PTY supervisor exited before it was ready ({status}); no terminal was started"
                );
            }
            Ok(None) => {}
            Err(error) => {
                let _ = handle.child.kill();
                let _ = handle.child.wait();
                handle.finished = true;
                return Err(error).context("Waiting for the managed PTY supervisor to be ready");
            }
        }
        if Instant::now() >= deadline {
            let _ = handle.child.kill();
            let _ = handle.child.wait();
            handle.finished = true;
            bail!(
                "the managed PTY supervisor did not become ready in time; no terminal was started"
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Wait for the owner's publication acknowledgement.
///
/// Returns `Ok(())` only when the supervisor validated the exact session
/// identity. The caller may then release the execution gate.
pub(crate) fn await_ack(directory: &Path, session: u32, deadline: Instant) -> Result<()> {
    loop {
        if let Some(value) = read_json(&directory.join(ACK_FILE)) {
            if value.get("session").and_then(Value::as_u64) == Some(u64::from(session)) {
                if value.get("ok").and_then(Value::as_bool) == Some(true) {
                    return Ok(());
                }
                let error = value
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("the supervisor rejected the session identity");
                bail!("The managed PTY supervisor rejected the session: {error}");
            }
        }
        if Instant::now() >= deadline {
            bail!("The managed PTY supervisor did not acknowledge the session identity in time; the user program was never started");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper process: the supervisor in tests. Running this test without the
    /// supervisor variable is a normal (vacuous) test pass.
    #[test]
    fn pty_supervisor_process_entry() {
        if std::env::var_os(DIR_ENV).is_none() {
            return;
        }
        match run_from_env() {
            Ok(_) => std::process::exit(0),
            Err(error) => {
                eprintln!("managed PTY supervisor: {error:#}");
                std::process::exit(70);
            }
        }
    }

    #[test]
    fn a_normal_finish_reflects_the_owner_terminal_status() {
        let recorded = json!({
            "status":"completed",
            "cleanup_complete":true,
            "output_capture_complete":true
        });
        let state = finalized_state(Some(&recorded), true);
        assert_eq!(state["status"], "completed", "{state}");
        assert_eq!(state["owner_lost"], false, "{state}");
        assert_eq!(state["owner_finalized"], true, "{state}");
        assert_eq!(state["cleanup_performed_by"], "owner", "{state}");
        assert_eq!(state["os_cleanup_complete"], true, "{state}");
        assert_eq!(state["output_finalized"], true, "{state}");
        assert_eq!(state["error"], Value::Null, "{state}");
    }

    #[test]
    fn a_missing_or_damaged_terminal_state_is_never_success() {
        for recorded in [
            None,
            Some(json!({"status":"completed!"})),
            Some(json!({})),
            Some(json!("not an object")),
        ] {
            let state = finalized_state(recorded.as_ref(), true);
            assert_eq!(state["status"], "failed", "{state}");
            assert_ne!(state["success"], true, "{state}");
            assert!(
                state["error"]
                    .as_str()
                    .is_some_and(|error| error.contains("no readable terminal status")),
                "{state}"
            );
        }
    }
}
