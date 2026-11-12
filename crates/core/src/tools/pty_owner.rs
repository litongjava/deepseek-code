//! Managed Linux PTY owner: durable task record, private cross-process control
//! channel and merged durable output.
//!
//! This is an internal increment. It does not register an `interaction` value,
//! is not reachable from `Bash` or `TaskInput`, and adds no tool name. The
//! transport is [`super::pty_transport`]; this module supplies the owner around
//! it.
//!
//! # Ownership model
//!
//! `start` creates exactly one **owner thread**. That thread creates exactly one
//! **accept thread** and joins it during shutdown. Requests pass from the accept
//! thread to the owner over a bounded `sync_channel(MAX_PENDING)`; the accept
//! thread tracks at most [`MAX_PENDING`] connections.
//!
//! `start` also returns an [`OwnerGuard`], which owns the owner thread's
//! `JoinHandle` and its task directory. `OwnerGuard::stop_and_join` writes the
//! documented cancel marker, waits (bounded) for the thread to finish, joins it
//! and returns the terminal state; `Drop` does the same bounded join. There is
//! no process-global registry that a caller could forget to drain.
//!
//! # Request lifetime
//!
//! Every request has a state cell: `QUEUED`, `CLAIMED` or
//! `CANCELLED_BEFORE_EXECUTION`. The owner claims with
//! `compare_exchange(QUEUED, CLAIMED)` immediately before its first effect; the
//! accept thread may report `not_executed` only when its own
//! `compare_exchange(QUEUED, CANCELLED_BEFORE_EXECUTION)` succeeds. When the
//! claim already happened, the accept thread waits a bounded grace period for
//! the real reply and otherwise reports `outcome:"unknown"` with
//! `may_have_been_written:true` — never a false "nothing was written".
//!
//! The client fixes one absolute deadline and transmits the *remaining* budget
//! immediately before the frame write, so connecting and framing are charged to
//! the caller rather than added to the window.
//!
//! # Files
//!
//! `terminal.json` is the **live owner descriptor**, written once at start:
//! generation, endpoint, the socket's `st_dev`/`st_ino`, token, pid, task and
//! session. `terminal-state.json` is the **final result**, written last:
//! status, exit code, cancellation, `cleanup_complete`, `final_drain_complete`
//! and `cleanup_error`.
//!
//! # Not claimed
//!
//! * `request_id` is a correlation id, not a dedup key; no durable dedup record
//!   exists, so a retry is never deduplicated.
//! * `SIGKILL` of the owner runs no `Drop`: input is refused afterwards (the
//!   lease is gone) but the child session is not cleaned by this module.
//! * PTY stdout and stderr are merged into `stdout.log`; `stderr.log` stays
//!   empty.
#![cfg(target_os = "linux")]

use anyhow::{anyhow, bail, ensure, Context, Result};
use portable_pty::ExitStatus;
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    os::{
        fd::{AsRawFd, BorrowedFd},
        unix::{
            fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
            net::{UnixListener, UnixStream},
        },
    },
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU8, Ordering},
        mpsc, Arc, Mutex,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use super::pty_transport::{self, PtyTransport, SpawnSpec, WriteRequest};
use super::{pty_gate, pty_supervisor};
use pty_supervisor::SupervisorHandle;

/// The `metadata.interaction` value internal PTY tasks carry. It is deliberately
/// not `pipe`, so the existing `TaskInput` path refuses these tasks.
pub(crate) const INTERACTION: &str = "pty_internal";

/// Request state cell values. The ordering matters: only `QUEUED` can move to
/// `CANCELLED_BEFORE_EXECUTION`, and only the owner can move `QUEUED` to
/// `CLAIMED`.
const QUEUED: u8 = 0;
const CLAIMED: u8 = 1;
const CANCELLED_BEFORE_EXECUTION: u8 = 2;

const MAX_DATA_BYTES: usize = pty_transport::MAX_INPUT_BYTES;
/// Data bound plus the JSON envelope.
const MAX_FRAME_BYTES: usize = MAX_DATA_BYTES + 1024;
const MAX_REPLY_BYTES: usize = 8 * 1024;
/// In-flight connections the accept thread keeps, and the command queue depth.
const MAX_PENDING: usize = 4;
const DRAIN_BYTES: usize = 16 * 1024;
/// Reads per pass, so a chatty child cannot starve control requests.
const DRAIN_READS_PER_PASS: usize = 32;
const DRAIN_SLICE: Duration = Duration::from_millis(50);
const LOOP_TICK: Duration = Duration::from_millis(5);
const ACCEPT_TICK: Duration = Duration::from_millis(5);
const HEARTBEAT: Duration = Duration::from_secs(1);
const SOCKET_IO_TIMEOUT: Duration = Duration::from_secs(2);
const DEFAULT_CALL_MS: u64 = 10_000;
const MAX_CALL_MS: u64 = 60_000;
const FINAL_DRAIN_BOUND: Duration = Duration::from_secs(5);
/// Longest single `poll` slice while connecting.
const CONNECT_SLICE: Duration = Duration::from_millis(25);
/// Pause before retrying a connect whose backlog was full.
const CONNECT_RETRY: Duration = Duration::from_millis(5);
/// How long the accept thread waits for the real reply after it discovers that
/// the request was already claimed for execution.
const EXECUTION_GRACE: Duration = Duration::from_secs(2);
/// How often the accept thread checks the cross-process cancel marker. A
/// marker therefore cancels an in-flight operation within one poll plus one
/// transport slice, independently of the request's own (possibly 60 s) budget.
const MARKER_POLL: Duration = Duration::from_millis(50);
/// Bound on writing a refusal to a peer while the accept thread shuts down.
const SHUTDOWN_REPLY_TIMEOUT: Duration = Duration::from_millis(250);
/// Bound on starting the independent supervisor before any child exists.
const SUPERVISOR_START_BOUND: Duration = Duration::from_secs(10);
/// Bound on the supervisor acknowledging the published session identity. The
/// user program is not started until this succeeds.
const PUBLICATION_BOUND: Duration = Duration::from_secs(10);
/// Bound on the supervisor exiting after the owner finished.
const SUPERVISOR_STOP_BOUND: Duration = Duration::from_secs(5);
const SESSION_FILE: &str = "session.json";

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Start a managed PTY task and return its record plus the owner guard.
///
/// The child runs on a real controlling terminal; its combined output is
/// appended to `<task dir>/stdout.log` by the owner thread. Callers must already
/// have decided the launch is authorized: this starts a process and must not be
/// reachable from model input. Dropping or joining the returned [`OwnerGuard`]
/// is what ends the owner normally.
pub(crate) fn start(
    spec: SpawnSpec,
    root: &Path,
    session: &str,
    description: &str,
    command_text: &str,
) -> Result<(Value, OwnerGuard)> {
    let session = uuid::Uuid::parse_str(session)
        .context("Invalid task session ID")?
        .to_string();
    let task = super::tasks::execute_for_shell_owner(
        "TaskCreate",
        json!({"subject":description,"metadata":{
            "managed_shell":true,
            "interaction":INTERACTION,
            "merged_output":true,
            "command":command_text,
            "started_ms":chrono::Utc::now().timestamp_millis()
        }}),
        root,
        &session,
    )?;
    let id = task["id"].as_str().context("Task ID missing")?.to_string();
    let directory = root.join("shell-tasks").join(&session).join(&id);
    // Register on the caller's thread, before the child exists and before the
    // owner thread is spawned. `has_active()` is therefore true for every
    // instant in which a started PTY is alive, and shutdown cannot observe a
    // gap while `start` is still running. The guard is moved into the owner and
    // released only after final cleanup.
    let active = ActiveOwner::enter(&id);
    #[cfg(test)]
    startup_probe::registered(&id);
    let mut supervisor: Option<SupervisorHandle> = None;
    let mut transport: Option<PtyTransport> = None;
    let prepared = (|| -> Result<super::background_shell::OwnerLease> {
        std::fs::create_dir_all(&directory)
            .with_context(|| format!("Creating the task directory for task {id}"))?;
        let lease = super::background_shell::take_lease(&directory, &id)?;
        std::fs::write(directory.join("owner.pid"), std::process::id().to_string())
            .with_context(|| format!("Recording the owner PID for task {id}"))?;
        // Merged output of a real terminal: stdout.log carries everything and
        // stderr.log exists only so readers see an empty, not a missing, stream.
        std::fs::File::create(directory.join("stderr.log"))
            .with_context(|| format!("Creating the merged-output placeholder for task {id}"))?;
        // The independent supervisor is ready before any child exists, so a
        // failure here leaves nothing to clean.
        supervisor = Some(pty_supervisor::spawn(
            &directory,
            std::process::id(),
            Instant::now() + SUPERVISOR_START_BOUND,
        )?);
        // The PTY child is the execution gate, not the user program: it cannot
        // run the user program until it receives GO, which happens only after
        // the supervisor acknowledged the session identity below.
        let mut gate = pty_gate::prepare(&spec)
            .with_context(|| format!("Preparing the execution gate for task {id}"))?;
        let spawned = pty_transport::spawn(&gate.spawn)
            .with_context(|| format!("Starting the terminal for task {id}"))?;
        let session_pid = spawned.process_id();
        transport = Some(spawned);
        // Test-only barrier: the gate is already a real PTY child, but nothing
        // was published and the user program was neither sent nor released.
        #[cfg(test)]
        prepublication_barrier::reach(&directory, &id, &session, session_pid)?;
        // The gate connects before anything is published. It cannot run the
        // user program until `release`, and closing this handshake makes it
        // exit without running the program.
        gate.accept_specification(Instant::now() + PUBLICATION_BOUND)
            .with_context(|| {
                format!("The execution gate for task {id} did not connect; the user program was never started")
            })?;
        let leader_start = pty_transport::process_start_tick(session_pid)
            .ok()
            .flatten()
            .unwrap_or(0);
        write_private(
            &directory.join(SESSION_FILE),
            json!({
                "version":1,
                "session":session_pid,
                "leader_start_tick":leader_start,
                "terminal_pid":session_pid,
                "published_ms":chrono::Utc::now().timestamp_millis()
            })
            .to_string()
            .as_bytes(),
        )?;
        pty_supervisor::await_ack(&directory, session_pid, Instant::now() + PUBLICATION_BOUND)
            .with_context(|| {
                format!(
                    "The managed PTY supervisor did not accept the session for task {id}; the user program was never started"
                )
            })?;
        // Ownership is established: release the gate. From here the supervisor
        // knows the exact session and can clean it if this process dies.
        gate.release()
            .with_context(|| format!("Releasing the execution gate for task {id}"))?;
        Ok(lease)
    })();
    let lease = match prepared {
        Ok(lease) => lease,
        Err(error) => {
            let terminal_pid = transport.as_ref().map(PtyTransport::process_id);
            let cleanup_error = match transport.take() {
                Some(transport) => transport.stop().err().map(|error| format!("{error:#}")),
                None => None,
            };
            // Dropping the gate closes its handshake, so a still-waiting gate
            // exits without running the user program.
            let supervisor_error = match supervisor.take() {
                Some(mut supervisor) => supervisor
                    .finish(false, Instant::now() + SUPERVISOR_STOP_BOUND)
                    .err()
                    .map(|error| format!("{error:#}")),
                None => None,
            };
            drop(active);
            return Err(fail_startup(
                root,
                &session,
                &id,
                &directory,
                terminal_pid,
                cleanup_error.or(supervisor_error),
                error,
            ));
        }
    };
    let transport = transport.expect("a successful preparation owns a transport");
    let terminal_pid = transport.process_id();
    let mut bound_socket: Option<PathBuf> = None;
    let endpoint = (|| -> Result<EndpointFiles> {
        let (endpoint_directory, socket, generation) = endpoint_paths();
        std::fs::create_dir_all(&endpoint_directory).with_context(|| {
            format!(
                "Creating the endpoint directory {}",
                endpoint_directory.display()
            )
        })?;
        std::fs::set_permissions(&endpoint_directory, std::fs::Permissions::from_mode(0o700))
            .with_context(|| {
                format!(
                    "Restricting the endpoint directory {}",
                    endpoint_directory.display()
                )
            })?;
        let listener =
            UnixListener::bind(&socket).with_context(|| format!("Binding {}", socket.display()))?;
        bound_socket = Some(socket.clone());
        std::fs::set_permissions(&socket, std::fs::Permissions::from_mode(0o600))
            .context("Restricting the endpoint socket")?;
        listener
            .set_nonblocking(true)
            .context("Making the endpoint nonblocking")?;
        // The exact identity of the bound socket: a replacement in a same-uid
        // directory gets a different inode and is refused by the client.
        let stat = rustix::fs::stat(&socket)
            .with_context(|| format!("Identifying the endpoint {}", socket.display()))?;
        let token = random_token()?;
        let record = json!({
            "version":1,
            "generation":generation,
            "endpoint":socket,
            "socket_dev":stat.st_dev,
            "socket_ino":stat.st_ino,
            "token":token,
            "owner_pid":std::process::id(),
            "pid":terminal_pid,
            "task_id":id,
            "session":session,
            "merged_output":true
        });
        write_private(
            &directory.join("terminal.json"),
            record.to_string().as_bytes(),
        )?;
        Ok(EndpointFiles {
            listener,
            socket,
            generation,
            token,
        })
    })();
    let endpoint = match endpoint {
        Ok(endpoint) => endpoint,
        Err(error) => {
            if let Some(socket) = bound_socket {
                let _ = std::fs::remove_file(socket);
            }
            let stop = transport.stop();
            let cleanup_error = stop.as_ref().err().map(|error| format!("{error:#}"));
            let supervisor_error = supervisor
                .take()
                .and_then(|mut supervisor| {
                    supervisor
                        .finish(false, Instant::now() + SUPERVISOR_STOP_BOUND)
                        .err()
                })
                .map(|error| format!("{error:#}"));
            drop(active);
            return Err(fail_startup(
                root,
                &session,
                &id,
                &directory,
                Some(terminal_pid),
                cleanup_error.or(supervisor_error),
                error,
            ));
        }
    };
    // Every fallible step after the child exists is routed through cleanup: no
    // path may leave a live session or a "running" record behind.
    let started = (|| -> Result<std::fs::File> {
        super::tasks::execute_for_shell_owner(
            "TaskUpdate",
            json!({"id":id,"status":"running"}),
            root,
            &session,
        )?;
        std::fs::write(directory.join("heartbeat"), b"running")
            .with_context(|| format!("Writing the heartbeat for task {id}"))?;
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(directory.join("stdout.log"))
            .with_context(|| format!("Opening the output log for task {id}"))
    })();
    let stdout = match started {
        Ok(stdout) => stdout,
        Err(error) => {
            let stop = transport.stop();
            let cleanup_error = stop.as_ref().err().map(|error| format!("{error:#}"));
            let _ = std::fs::remove_file(&endpoint.socket);
            let supervisor_error = supervisor
                .take()
                .and_then(|mut supervisor| {
                    supervisor
                        .finish(false, Instant::now() + SUPERVISOR_STOP_BOUND)
                        .err()
                })
                .map(|error| format!("{error:#}"));
            drop(active);
            return Err(fail_startup(
                root,
                &session,
                &id,
                &directory,
                Some(terminal_pid),
                cleanup_error.or(supervisor_error),
                error,
            ));
        }
    };
    let transport = Arc::new(transport);
    let shared = Arc::new(OwnerShared::new());
    let (sender, commands) = mpsc::sync_channel(MAX_PENDING);
    let owner = Owner {
        transport: transport.clone(),
        listener: endpoint.listener,
        root: root.to_owned(),
        session: session.clone(),
        id: id.clone(),
        directory: directory.clone(),
        socket_path: endpoint.socket.clone(),
        terminal_state_path: directory.join("terminal-state.json"),
        generation: Arc::new(endpoint.generation),
        token: Arc::new(endpoint.token),
        shared: shared.clone(),
        commands,
        sender,
        output: OutputSink {
            file: stdout,
            #[cfg(test)]
            shared: shared.clone(),
        },
    };
    let supervisor = Arc::new(Mutex::new(supervisor.take()));
    let cleanup_result: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let wrapper = OwnerRun {
        owner: Some(owner),
        active: Some(active),
        cleanup: cleanup_result.clone(),
        supervisor: supervisor.clone(),
    };
    let handle = match spawn_owner_thread(wrapper, lease, &id) {
        Ok(handle) => handle,
        Err(error) => {
            let _ = std::fs::remove_file(&endpoint.socket);
            let cleanup_error = lock(&cleanup_result).clone();
            return Err(fail_startup(
                root,
                &session,
                &id,
                &directory,
                Some(terminal_pid),
                cleanup_error,
                anyhow::Error::new(error).context("Spawning the managed PTY owner thread"),
            ));
        }
    };
    let guard = OwnerGuard {
        id: id.clone(),
        directory: directory.clone(),
        join: Some(handle),
        shared,
        supervisor,
    };
    let value = json!({
        "task_id": id,
        "subject": description,
        "status": "running",
        "interaction": INTERACTION,
        "merged_output": true,
        "stdout_file": directory.join("stdout.log"),
        "stderr_file": directory.join("stderr.log"),
        "terminal_pid": terminal_pid,
        "endpoint": endpoint.socket,
        "message": "Internal managed PTY task started. It is not exposed as a tool interaction yet."
    });
    Ok((value, guard))
}

/// Spawn the owner thread, or fail after synchronously releasing the child and
/// the registration through [`OwnerRun`]'s drop. The forced-failure seam exists
/// only because a real `std::thread::Builder::spawn` failure cannot be produced
/// without exhausting process resources.
fn spawn_owner_thread(
    wrapper: OwnerRun,
    lease: super::background_shell::OwnerLease,
    id: &str,
) -> std::io::Result<JoinHandle<()>> {
    #[cfg(test)]
    if startup_injection::take_spawn_failure() {
        drop(wrapper);
        return Err(std::io::Error::other("injected owner thread spawn failure"));
    }
    std::thread::Builder::new()
        .name(format!("dcl-pty-{id}"))
        .spawn(move || wrapper.run(lease))
}

/// The not-yet-started owner plus its registration guard.
///
/// Field order and `Drop` guarantee that a failed spawn stops the child session
/// before the active-owner count is decremented, and that starting the thread
/// transfers both fields with no gap.
struct OwnerRun {
    owner: Option<Owner>,
    active: Option<ActiveOwner>,
    cleanup: Arc<Mutex<Option<String>>>,
    supervisor: Arc<Mutex<Option<SupervisorHandle>>>,
}

impl OwnerRun {
    fn run(mut self, lease: super::background_shell::OwnerLease) {
        let owner = self.owner.take().expect("the owner is present");
        let active = self.active.take().expect("the registration is present");
        owner.run(lease, active);
    }
}

impl Drop for OwnerRun {
    fn drop(&mut self) {
        // Only the never-started path is cleaned here; a started owner is
        // cleaned by `OwnerGuard`, which knows whether it finalized.
        if let Some(owner) = self.owner.take() {
            let result = owner.transport.stop();
            let mut error = result.as_ref().err().map(|error| format!("{error:#}"));
            if let Some(mut supervisor) = lock(&self.supervisor).take() {
                if let Err(supervisor_error) =
                    supervisor.finish(false, Instant::now() + SUPERVISOR_STOP_BOUND)
                {
                    let text = format!("{supervisor_error:#}");
                    error = Some(match error {
                        Some(existing) => format!("{existing}; {text}"),
                        None => text,
                    });
                }
            }
            *lock(&self.cleanup) = error;
        }
    }
}

/// Owned lifecycle handle for one managed PTY task.
///
/// The guard owns the owner thread's `JoinHandle`. Dropping it performs the
/// bounded normal shutdown described in [`OwnerGuard::stop_and_join`]; it does
/// not claim to handle `SIGKILL`, which runs no `Drop` at all.
pub(crate) struct OwnerGuard {
    id: String,
    directory: PathBuf,
    join: Option<JoinHandle<()>>,
    shared: Arc<OwnerShared>,
    supervisor: Arc<Mutex<Option<SupervisorHandle>>>,
}

impl OwnerGuard {
    pub(crate) fn task_id(&self) -> &str {
        &self.id
    }

    /// Ask the owner to stop, wait for it to finish, join it, and return the
    /// terminal state it recorded.
    ///
    /// The shared stop/cancel flags reach a blocked write or read immediately;
    /// the file is written as well because it is the documented cross-process
    /// signal. Any persistence failure is an error, never a success result.
    /// The independent supervisor is not reported as cleanly stopped unless it
    /// actually exited.
    pub(crate) fn stop_and_join(&mut self, deadline: Instant) -> Result<Value> {
        self.shared.stop.store(true, Ordering::Release);
        self.shared.cancel.store(true, Ordering::Release);
        let _ = std::fs::write(self.directory.join("cancel"), b"cancel");
        self.join(deadline)?;
        // Only an owner that reached its finalizer may tell the supervisor to
        // exit without cleaning the session.
        let normal = lock(&self.shared.report).is_some();
        let supervisor_error = self.finish_supervisor(normal);
        let report = self.report();
        match supervisor_error {
            None => report,
            Some(error) => match report {
                Ok(_) => Err(anyhow!(error)),
                Err(report_error) => Err(report_error.context(error)),
            },
        }
    }

    /// Stop the supervisor after the owner thread ended.
    fn finish_supervisor(&self, normal: bool) -> Option<String> {
        let mut cell = lock(&self.supervisor);
        let handle = cell.as_mut()?;
        match handle.finish(normal, Instant::now() + SUPERVISOR_STOP_BOUND) {
            Ok(()) => {
                *cell = None;
                None
            }
            Err(error) => Some(format!(
                "The managed PTY supervisor for task {} could not be stopped cleanly: {error:#}",
                self.id
            )),
        }
    }

    /// Join the owner thread without asking it to stop (for example after a
    /// `TaskStop` that already wrote the cancel marker).
    pub(crate) fn join(&mut self, deadline: Instant) -> Result<()> {
        let Some(handle) = self.join.take() else {
            return Ok(());
        };
        while !handle.is_finished() {
            if Instant::now() >= deadline {
                self.join = Some(handle);
                bail!(
                    "the managed PTY owner for task {} did not exit in time",
                    self.id
                );
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        handle.join().map_err(|_| {
            anyhow::anyhow!("the managed PTY owner thread for task {} panicked", self.id)
        })?;
        Ok(())
    }

    /// The final report, or an error naming every persistence path that failed.
    ///
    /// `Ok` is only possible when both the terminal metadata and the durable
    /// task record were written. A task-record failure never degrades into a
    /// successful-looking value.
    fn report(&self) -> Result<Value> {
        let report = lock(&self.shared.report).clone();
        let Some(report) = report else {
            let text = std::fs::read_to_string(self.directory.join("terminal-state.json"))
                .context("Reading the terminal state after shutdown")?;
            return serde_json::from_str(&text)
                .context("Parsing the terminal state after shutdown");
        };
        if report.terminal_state_persisted && report.task_record_updated {
            let mut value = report.terminal;
            value["terminal_state_persisted"] = json!(true);
            value["task_record_updated"] = json!(true);
            return Ok(value);
        }
        let status = report
            .terminal
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let cleanup = report
            .terminal
            .get("cleanup_complete")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let output = report
            .terminal
            .get("output_capture_complete")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let drain = report
            .terminal
            .get("final_drain_complete")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let terminal_error = report
            .terminal_state_error
            .as_deref()
            .unwrap_or("no terminal-state error");
        let record_error = report
            .task_record_error
            .as_deref()
            .unwrap_or("no task-record error");
        Err(anyhow!(
            "The managed PTY terminal for task {} did not persist a complete result. Runtime status: {status}; cleanup_complete: {cleanup}; output_capture_complete: {output}; final_drain_complete: {drain}. terminal-state persisted: {} ({terminal_error}). task record updated: {} ({record_error}).",
            self.id,
            report.terminal_state_persisted,
            report.task_record_updated,
        ))
    }
}

impl Drop for OwnerGuard {
    fn drop(&mut self) {
        // Best-effort normal shutdown. A failure is logged, never reported as
        // success, and `SIGKILL` is out of scope.
        if let Err(error) = self.stop_and_join(Instant::now() + Duration::from_secs(15)) {
            tracing::warn!(%error, task = %self.id, "Managed PTY owner shutdown failed");
        }
    }
}

fn fail_startup(
    root: &Path,
    session: &str,
    id: &str,
    directory: &Path,
    terminal_pid: Option<u32>,
    cleanup_error: Option<String>,
    error: anyhow::Error,
) -> anyhow::Error {
    let cleanup_complete = cleanup_error.is_none();
    let terminal = json!({
        "version": 1,
        "status": "failed",
        "startup_failed": true,
        "cleanup_complete": cleanup_complete,
        "cleanup_error": cleanup_error,
        "output_capture_complete": false,
        "output_error": "the managed PTY task failed during startup",
        "final_drain_complete": false,
        "merged_output": true,
        "terminal_pid": terminal_pid,
        "finished_ms": chrono::Utc::now().timestamp_millis()
    });
    let _ = write_private_atomic(
        &directory.join("terminal-state.json"),
        terminal.to_string().as_bytes(),
    );
    let output = json!({
        "is_error": true,
        "startup_failed": true,
        "error": format!("{error:#}"),
        "cleanup_complete": cleanup_complete,
        "cleanup_error": cleanup_error
    });
    match super::tasks::execute_for_shell_owner(
        "TaskUpdate",
        json!({"id":id,"status":"failed","output":output.to_string()}),
        root,
        session,
    ) {
        Ok(_) => error,
        Err(record_error) => anyhow::anyhow!(
            "Managed PTY startup failed: {error:#}. Recording the failure also failed: {record_error:#}; the task record may still show its previous status."
        ),
    }
}

/// Files and identities the owner needs for its endpoint.
struct EndpointFiles {
    listener: UnixListener,
    socket: PathBuf,
    generation: String,
    token: String,
}

/// One control request routed from the accept thread to the owner.
///
/// `stop` is deliberately absent: an admitted Stop is claimed and applied by
/// the accept thread at admission, so it can never be a queued command whose
/// outcome might later be reclassified.
enum Command {
    Write {
        request_id: String,
        data: Vec<u8>,
        /// Send the terminal's VEOF after the data, if the data was fully
        /// accepted. A partial write never triggers it.
        then_eof: bool,
        deadline: Instant,
        cancel: Arc<AtomicBool>,
        state: Arc<AtomicU8>,
        reply: mpsc::SyncSender<Value>,
    },
    Eof {
        request_id: String,
        deadline: Instant,
        cancel: Arc<AtomicBool>,
        state: Arc<AtomicU8>,
        reply: mpsc::SyncSender<Value>,
    },
    Resize {
        request_id: String,
        rows: u16,
        cols: u16,
        deadline: Instant,
        state: Arc<AtomicU8>,
        reply: mpsc::SyncSender<Value>,
    },
}

impl Command {
    fn request_id(&self) -> &str {
        match self {
            Self::Write { request_id, .. }
            | Self::Eof { request_id, .. }
            | Self::Resize { request_id, .. } => request_id,
        }
    }
}

struct ActiveOwner {
    #[cfg(test)]
    id: String,
}
impl ActiveOwner {
    fn enter(id: &str) -> Self {
        super::background_shell::enter_owner();
        #[cfg(test)]
        registration_probe::enter(id);
        let _ = id;
        Self {
            #[cfg(test)]
            id: id.to_owned(),
        }
    }
}
impl Drop for ActiveOwner {
    fn drop(&mut self) {
        super::background_shell::leave_owner();
        #[cfg(test)]
        registration_probe::leave(&self.id);
    }
}

/// State shared by the owner, the accept thread and the guard.
///
/// `stop` terminates the owner; `cancel` cancels the active operation and makes
/// the owner refuse queued effects. Both are set directly by an in-process
/// guard and by the accept thread when it observes a stop source.
struct OwnerShared {
    stop: AtomicBool,
    cancel: AtomicBool,
    report: Mutex<Option<FinalReport>>,
    #[cfg(test)]
    output_fault: AtomicU8,
    #[cfg(test)]
    final_tail: Mutex<Vec<u8>>,
    #[cfg(test)]
    fail_task_update: AtomicBool,
}

impl OwnerShared {
    fn new() -> Self {
        Self {
            stop: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
            report: Mutex::new(None),
            #[cfg(test)]
            output_fault: AtomicU8::new(0),
            #[cfg(test)]
            final_tail: Mutex::new(Vec::new()),
            #[cfg(test)]
            fail_task_update: AtomicBool::new(false),
        }
    }

    #[cfg(test)]
    fn take_final_tail(&self) -> Vec<u8> {
        std::mem::take(&mut *lock(&self.final_tail))
    }

    #[cfg(test)]
    fn fault(&self, phase: Phase, flush: bool) -> bool {
        let mode = self.output_fault.load(Ordering::Acquire);
        matches!(
            (mode, phase, flush),
            (1, Phase::Final, false)
                | (2, Phase::Final, true)
                | (3, Phase::Drain, false)
                | (4, Phase::Drain, true)
        )
    }
}

#[derive(Clone)]
struct FinalReport {
    terminal: Value,
    terminal_state_persisted: bool,
    terminal_state_error: Option<String>,
    task_record_updated: bool,
    task_record_error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Drain,
    Final,
}

/// The append-only merged output log. Test-only faults fail one phase so the
/// real write/flush error paths can be exercised without touching the child.
struct OutputSink {
    file: std::fs::File,
    #[cfg(test)]
    shared: Arc<OwnerShared>,
}

impl OutputSink {
    fn append(&mut self, bytes: &[u8], phase: Phase) -> std::result::Result<(), String> {
        let _ = phase;
        #[cfg(test)]
        if self.shared.fault(phase, false) {
            return Err(format!("injected output write failure during {phase:?}"));
        }
        self.file
            .write_all(bytes)
            .map_err(|error| format!("appending PTY output to the task log: {error}"))
    }

    fn flush(&mut self, phase: Phase) -> std::result::Result<(), String> {
        let _ = phase;
        #[cfg(test)]
        if self.shared.fault(phase, true) {
            return Err(format!("injected output flush failure during {phase:?}"));
        }
        self.file
            .flush()
            .map_err(|error| format!("flushing PTY output to the task log: {error}"))
    }
}

#[derive(Default)]
struct OutputCapture {
    error: Option<String>,
}

impl OutputCapture {
    fn record(&mut self, error: String) {
        if self.error.is_none() {
            self.error = Some(error);
        }
    }
}

enum Drain {
    Eof,
    Idle,
    CaptureFailed,
}

/// The owner thread: transport, lease and final state.
struct Owner {
    transport: Arc<PtyTransport>,
    listener: UnixListener,
    root: PathBuf,
    session: String,
    id: String,
    directory: PathBuf,
    socket_path: PathBuf,
    terminal_state_path: PathBuf,
    generation: Arc<String>,
    token: Arc<String>,
    shared: Arc<OwnerShared>,
    commands: mpsc::Receiver<Command>,
    sender: mpsc::SyncSender<Command>,
    output: OutputSink,
}

impl Owner {
    fn run(mut self, lease: super::background_shell::OwnerLease, active: ActiveOwner) {
        let _active = active;
        let _lease = lease;
        let shutdown = Arc::new(AtomicBool::new(false));
        let accept = {
            let shared = AcceptShared {
                listener: self.listener.try_clone().expect("the listener is clonable"),
                token: self.token.clone(),
                generation: self.generation.clone(),
                task_id: Arc::new(self.id.clone()),
                session: Arc::new(self.session.clone()),
                sender: self.sender.clone(),
                shutdown: shutdown.clone(),
                owner: self.shared.clone(),
                cancel_marker: self.directory.join("cancel"),
            };
            std::thread::Builder::new()
                .name(format!("dcl-pty-accept-{}", self.id))
                .spawn(move || accept_loop(shared))
        };
        let accept = match accept {
            Ok(handle) => handle,
            Err(error) => {
                // Without an accept thread nothing can be controlled: clean up
                // and report the failure instead of pretending to run.
                let stop = self.transport.stop();
                let mut output = OutputCapture::default();
                output.record(format!("the endpoint thread could not start: {error}"));
                self.finalize(false, stop, output, false);
                return;
            }
        };
        let mut cancelled = false;
        let mut heartbeat = Instant::now();
        let mut output = OutputCapture::default();
        loop {
            // Check stop sources before draining and before popping any queued
            // command: after a stop request no new effect may start.
            if self.stop_requested() {
                cancelled = true;
                break;
            }
            match self.drain(&mut output) {
                Drain::Eof => break,
                Drain::Idle => {}
                Drain::CaptureFailed => break,
            }
            if self.stop_requested() {
                cancelled = true;
                break;
            }
            if let Ok(command) = self.commands.try_recv() {
                if self.serve(command) {
                    cancelled = true;
                }
            }
            if heartbeat.elapsed() >= HEARTBEAT {
                let _ = std::fs::write(self.directory.join("heartbeat"), b"running");
                heartbeat = Instant::now();
            }
            if self.stop_requested() {
                cancelled = true;
                break;
            }
            std::thread::sleep(LOOP_TICK);
        }
        // No new work is admitted from here on; the accept thread refuses what
        // it still holds and returns.
        shutdown.store(true, Ordering::Release);
        if accept.join().is_err() {
            output.record("the endpoint thread panicked".to_owned());
        }
        #[cfg(test)]
        pre_stop_gate::wait(&self.id);
        let stop = self.transport.stop();
        let final_drain_complete = if output.error.is_none() {
            self.final_drain(&mut output)
        } else {
            false
        };
        self.finalize(cancelled, stop, output, final_drain_complete);
    }

    fn stop_requested(&self) -> bool {
        self.shared.stop.load(Ordering::Acquire) || self.directory.join("cancel").exists()
    }

    /// Append available terminal output. A capture failure is not a child
    /// failure; the caller finalizes with the output facts kept separate.
    fn drain(&mut self, output: &mut OutputCapture) -> Drain {
        for _ in 0..DRAIN_READS_PER_PASS {
            let outcome = match self.transport.read_bytes(
                DRAIN_BYTES,
                Instant::now() + DRAIN_SLICE,
                Some(&self.shared.stop),
            ) {
                Ok(outcome) => outcome,
                Err(error) => {
                    output.record(format!("reading the terminal output failed: {error:#}"));
                    return Drain::CaptureFailed;
                }
            };
            if !outcome.bytes.is_empty() {
                if let Err(error) = self.output.append(&outcome.bytes, Phase::Drain) {
                    output.record(error);
                    return Drain::CaptureFailed;
                }
                if let Err(error) = self.output.flush(Phase::Drain) {
                    output.record(error);
                    return Drain::CaptureFailed;
                }
            }
            if outcome.eof {
                return Drain::Eof;
            }
            if outcome.bytes.is_empty() {
                return Drain::Idle;
            }
        }
        Drain::Idle
    }

    /// Read the tail that was still queued when the session was stopped.
    /// Returns whether the tail is provably complete.
    ///
    /// A write or flush failure is recorded and stops capture immediately;
    /// bytes are never retried, so a failed chunk cannot be duplicated.
    fn final_drain(&mut self, output: &mut OutputCapture) -> bool {
        #[cfg(test)]
        {
            let injected = self.shared.take_final_tail();
            if !injected.is_empty() {
                if let Err(error) = self.output.append(&injected, Phase::Final) {
                    output.record(error);
                    return false;
                }
                if let Err(error) = self.output.flush(Phase::Final) {
                    output.record(error);
                    return false;
                }
            }
        }
        if output.error.is_some() {
            return false;
        }
        let deadline = Instant::now() + FINAL_DRAIN_BOUND;
        loop {
            if Instant::now() >= deadline {
                return false;
            }
            let outcome =
                match self
                    .transport
                    .read_bytes(DRAIN_BYTES, Instant::now() + DRAIN_SLICE, None)
                {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        output.record(format!("the final output drain failed to read: {error:#}"));
                        return false;
                    }
                };
            if !outcome.bytes.is_empty() {
                if let Err(error) = self.output.append(&outcome.bytes, Phase::Final) {
                    output.record(error);
                    return false;
                }
                if let Err(error) = self.output.flush(Phase::Final) {
                    output.record(error);
                    return false;
                }
                continue;
            }
            if outcome.eof {
                return true;
            }
            // An empty queue with cleanup unconfirmed is not a complete tail.
            return !outcome.cleanup_pending;
        }
    }

    /// Handle one control request. Returns `true` when a stop was requested.
    fn serve(&self, command: Command) -> bool {
        #[cfg(test)]
        let request_id = command.request_id().to_owned();
        #[cfg(test)]
        serve_gate::wait(&request_id);
        let outcome = self.serve_command(command);
        #[cfg(test)]
        claim_probe::finished(&request_id);
        outcome
    }

    fn claim_state(state: &AtomicU8, request_id: &str) -> bool {
        let _ = request_id;
        let claimed = state
            .compare_exchange(QUEUED, CLAIMED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok();
        #[cfg(test)]
        if claimed {
            claim_probe::claimed(request_id);
        }
        claimed
    }

    /// Claim a non-stop effect. A stop source that arrived while the command
    /// was queued refuses it here, before any effect; the proven
    /// `QUEUED -> CANCELLED` transition is what makes `not_executed` honest.
    fn claim_non_stop(&self, state: &AtomicU8, request_id: &str) -> bool {
        if self.stop_requested() || self.shared.cancel.load(Ordering::Acquire) {
            let _ = state.compare_exchange(
                QUEUED,
                CANCELLED_BEFORE_EXECUTION,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
            return false;
        }
        Self::claim_state(state, request_id)
    }

    fn serve_command(&self, command: Command) -> bool {
        match command {
            Command::Write {
                request_id,
                data,
                then_eof,
                deadline,
                cancel,
                state,
                reply,
            } => {
                if !self.claim_non_stop(&state, &request_id) {
                    // Proven cancelled before execution: nothing was written.
                    let _ = reply.send(not_executed(&request_id, "write", "nothing was written"));
                    return false;
                }
                #[cfg(test)]
                let mut progress = {
                    let id = request_id.clone();
                    move |count: usize| claim_probe::accepted(&id, count)
                };
                #[cfg(test)]
                let on_accepted: Option<&mut dyn FnMut(usize)> = Some(&mut progress);
                #[cfg(not(test))]
                let on_accepted: Option<&mut dyn FnMut(usize)> = None;
                let outcome = self.transport.write_bytes(WriteRequest {
                    data: &data,
                    deadline,
                    cancelled: Some(cancel.as_ref()),
                    on_accepted,
                });
                let value = match outcome {
                    Ok(outcome) => {
                        // VEOF is a separate terminal operation and only runs
                        // when the whole payload was accepted; a partial write
                        // must not turn its tail into an end-of-file.
                        let mut value = json!({
                            "ok": true,
                            "request_id": request_id,
                            "operation": "write",
                            "bytes_written": outcome.accepted,
                            "bytes_remaining": outcome.remaining,
                            "write_complete": outcome.remaining == 0,
                            "timed_out": outcome.timed_out,
                            "cancelled": outcome.cancelled,
                            "closed": outcome.closed
                        });
                        if then_eof && outcome.remaining == 0 {
                            match self.transport.send_eof(deadline, Some(cancel.as_ref())) {
                                Ok(eof) => {
                                    value["eof_sent"] = json!(true);
                                    value["veof_byte"] = json!(eof.byte);
                                    value["stdin_closed"] = json!(false);
                                }
                                Err(error) => {
                                    value["eof_sent"] = json!(false);
                                    value["eof_error"] = json!(error.to_string());
                                    value["stdin_closed"] = json!(false);
                                }
                            }
                        } else if then_eof {
                            value["eof_sent"] = json!(false);
                            value["eof_error"] =
                                json!("VEOF was not sent because the write was incomplete");
                        }
                        value
                    }
                    Err(error) => json!({
                        "ok": false,
                        "request_id": request_id,
                        "operation": "write",
                        "outcome": "unknown",
                        "may_have_been_written": true,
                        "error": format!("{error:#}")
                    }),
                };
                let _ = reply.send(value);
                false
            }
            Command::Eof {
                request_id,
                deadline,
                cancel,
                state,
                reply,
            } => {
                if !self.claim_non_stop(&state, &request_id) {
                    let _ = reply.send(not_executed(&request_id, "eof", "no VEOF was sent"));
                    return false;
                }
                let value = match self.transport.send_eof(deadline, Some(cancel.as_ref())) {
                    Ok(outcome) => json!({
                        "ok": true,
                        "request_id": request_id,
                        "operation": "eof",
                        "veof_byte": outcome.byte,
                        "stdin_closed": false
                    }),
                    Err(error) => json!({
                        "ok": false,
                        "request_id": request_id,
                        "operation": "eof",
                        "error_kind": "eof_unsupported",
                        "error": error.to_string()
                    }),
                };
                let _ = reply.send(value);
                false
            }
            Command::Resize {
                request_id,
                rows,
                cols,
                deadline,
                state,
                reply,
            } => {
                // The caller's absolute deadline is checked before the claim
                // and again immediately before the side effect. A paused
                // accept loop cannot make an expired resize apply late.
                if Instant::now() >= deadline {
                    let _ = state.compare_exchange(
                        QUEUED,
                        CANCELLED_BEFORE_EXECUTION,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                    let _ = reply.send(not_executed(
                        &request_id,
                        "resize",
                        "the caller's deadline passed before the owner executed it; the terminal was not resized",
                    ));
                    return false;
                }
                if !self.claim_non_stop(&state, &request_id) {
                    let _ = reply.send(not_executed(
                        &request_id,
                        "resize",
                        "the request was cancelled before the owner executed it; the terminal was not resized",
                    ));
                    return false;
                }
                if Instant::now() >= deadline {
                    let _ = reply.send(not_executed(
                        &request_id,
                        "resize",
                        "the caller's deadline passed immediately before the resize; the terminal was not resized",
                    ));
                    return false;
                }
                let value = match self.transport.resize(rows, cols) {
                    Ok(outcome) => json!({
                        "ok": true,
                        "request_id": request_id,
                        "operation": "resize",
                        "rows": outcome.rows,
                        "cols": outcome.cols
                    }),
                    Err(error) => json!({
                        "ok": false,
                        "request_id": request_id,
                        "operation": "resize",
                        "error": format!("{error:#}")
                    }),
                };
                let _ = reply.send(value);
                false
            }
        }
    }

    /// Record the terminal state and release the endpoint.
    ///
    /// Called exactly once, after the session cleanup finished and the final
    /// drain ran. Child cleanup, output capture and metadata persistence stay
    /// separate facts: an output failure is never labelled a cleanup failure,
    /// and no persistence failure becomes a success.
    ///
    /// `write_all`/`flush` completion is not `fsync` durability; no `fsync` is
    /// issued, so a successful write means the kernel accepted the bytes, not
    /// that they survive a power loss.
    fn finalize(
        &mut self,
        cancelled: bool,
        cleanup: Result<Option<ExitStatus>>,
        capture: OutputCapture,
        final_drain_complete: bool,
    ) {
        let _ = std::fs::remove_file(&self.socket_path);
        let cleanup_error = cleanup.as_ref().err().map(|error| format!("{error:#}"));
        let cleanup_complete = cleanup_error.is_none();
        let exit = cleanup.as_ref().ok().and_then(|status| status.as_ref());
        let exit_code = exit.map(ExitStatus::exit_code);
        let signal = exit.and_then(|status| status.signal().map(str::to_owned));
        let output_error = capture.error;
        let output_complete = final_drain_complete && output_error.is_none();
        let status = terminal_status(cancelled, cleanup_complete, output_complete, exit_code);
        let terminal = json!({
            "version": 1,
            "status": status,
            "exit_code": exit_code,
            "signal": signal,
            "cancelled": cancelled,
            "cleanup_complete": cleanup_complete,
            "cleanup_error": cleanup_error.clone(),
            "output_capture_complete": output_complete,
            "output_error": output_error.clone(),
            "final_drain_complete": final_drain_complete,
            "merged_output": true,
            "finished_ms": chrono::Utc::now().timestamp_millis()
        });
        // The terminal metadata is written first so the durable task record can
        // state whether it persisted. If it did not, the recorded task status
        // is `failed`: a missing terminal result can never look like success.
        let terminal_write =
            write_private_atomic(&self.terminal_state_path, terminal.to_string().as_bytes());
        let (terminal_state_persisted, mut terminal_state_error) = match terminal_write {
            Ok(()) => (true, None),
            Err(error) => (false, Some(format!("{error:#}"))),
        };
        let recorded_status = if terminal_state_persisted {
            status
        } else {
            "failed"
        };
        let success = status == "completed" && terminal_state_persisted;
        let output = json!({
            "success": success,
            "is_error": !success,
            "status": recorded_status,
            "runtime_status": status,
            "cancelled": cancelled,
            "exit_code": exit_code,
            "signal": signal,
            "cleanup_complete": cleanup_complete,
            "cleanup_error": cleanup_error,
            "output_capture_complete": output_complete,
            "output_error": output_error,
            "final_drain_complete": final_drain_complete,
            "terminal_state_persisted": terminal_state_persisted,
            "terminal_state_error": terminal_state_error,
            "merged_output": true
        });
        let (task_record_updated, task_record_error) =
            match self.update_task(recorded_status, &output) {
                Ok(()) => (true, None),
                Err(error) => (false, Some(format!("{error:#}"))),
            };
        if let Some(error) = &task_record_error {
            tracing::warn!(%error, task = %self.id, "Recording the terminal PTY state failed");
        }
        // If the task record could not be updated, make the saved metadata say
        // so when the filesystem still allows it. A failure here is reported but
        // does not invent a new runtime status.
        let mut terminal = terminal;
        if !task_record_updated {
            terminal["task_record_updated"] = json!(false);
            terminal["task_record_error"] = json!(task_record_error);
            terminal["persistence_error"] = json!(
                "the durable task record was not updated after the terminal state was written"
            );
            if let Err(error) =
                write_private_atomic(&self.terminal_state_path, terminal.to_string().as_bytes())
            {
                terminal_state_error = Some(match terminal_state_error.take() {
                    Some(original) => format!(
                        "{original}; the record-failure metadata could not be refreshed: {error:#}"
                    ),
                    None => {
                        format!("the record-failure metadata could not be refreshed: {error:#}")
                    }
                });
            }
        } else {
            terminal["task_record_updated"] = json!(true);
        }
        *lock(&self.shared.report) = Some(FinalReport {
            terminal,
            terminal_state_persisted,
            terminal_state_error,
            task_record_updated,
            task_record_error,
        });
    }

    fn update_task(&self, status: &str, output: &Value) -> Result<()> {
        #[cfg(test)]
        if self.shared.fail_task_update.load(Ordering::Acquire) {
            bail!("injected task record update failure");
        }
        super::tasks::execute_for_shell_owner(
            "TaskUpdate",
            json!({"id":self.id,"status":status,"output":output.to_string()}),
            &self.root,
            &self.session,
        )?;
        Ok(())
    }
}

fn not_executed(request_id: &str, operation: &str, detail: &str) -> Value {
    let mut value = json!({
        "ok": false,
        "request_id": request_id,
        "operation": operation,
        "outcome": "not_executed",
        "error": format!("the request was cancelled before the owner executed it; {detail}")
    });
    if matches!(operation, "write" | "eof") {
        value["may_have_been_written"] = json!(false);
    } else {
        value["may_have_been_applied"] = json!(false);
    }
    value
}

/// The reply for a Stop that was claimed and applied by the accept thread.
fn stop_accepted(request_id: &str) -> Value {
    json!({
        "ok": true,
        "request_id": request_id,
        "operation": "stop",
        "cancellation_requested": true,
        "confirmed": false
    })
}

/// The terminal status a task records. A cleanup failure, an incomplete output
/// capture or a missing tail can never look like success.
fn terminal_status(
    cancelled: bool,
    cleanup_complete: bool,
    output_complete: bool,
    exit_code: Option<u32>,
) -> &'static str {
    if !cleanup_complete || !output_complete {
        return "failed";
    }
    if cancelled {
        return "cancelled";
    }
    match exit_code {
        Some(0) => "completed",
        _ => "failed",
    }
}

struct AcceptShared {
    listener: UnixListener,
    token: Arc<String>,
    generation: Arc<String>,
    task_id: Arc<String>,
    session: Arc<String>,
    sender: mpsc::SyncSender<Command>,
    shutdown: Arc<AtomicBool>,
    owner: Arc<OwnerShared>,
    cancel_marker: PathBuf,
}

/// One connection whose request is queued but whose reply is not written yet.
struct Pending {
    stream: UnixStream,
    reply: mpsc::Receiver<Value>,
    state: Arc<AtomicU8>,
    /// Set by the accept thread when the caller's deadline passes, the peer
    /// disappears, or a stop is admitted. The transport observes it within one
    /// poll slice, so a blocked write ends with its accepted prefix.
    cancel: Arc<AtomicBool>,
    deadline: Instant,
    /// Set once the request is known to be claimed for execution; the accept
    /// thread then waits for the real reply instead of cancelling.
    grace: Option<Instant>,
    request_id: String,
    operation: &'static str,
}

impl Pending {
    /// What can honestly be said once this request is out of our hands.
    fn unknown_reply(&self) -> Value {
        match self.operation {
            "write" | "eof" => json!({
                "ok": false,
                "request_id": self.request_id,
                "operation": self.operation,
                "outcome": "unknown",
                "may_have_been_written": true,
                "error": "the owner is still executing this request; part of it may already have reached the terminal, so do not repeat it blindly"
            }),
            _ => json!({
                "ok": false,
                "request_id": self.request_id,
                "operation": self.operation,
                "outcome": "unknown",
                "may_have_been_applied": true,
                "error": "the owner is still executing this request; it may already have been applied"
            }),
        }
    }

    fn not_executed_reply(&self) -> Value {
        match self.operation {
            "write" | "eof" => json!({
                "ok": false,
                "request_id": self.request_id,
                "operation": self.operation,
                "outcome": "not_executed",
                "may_have_been_written": false,
                "error": "the request was cancelled before execution; nothing was written"
            }),
            _ => json!({
                "ok": false,
                "request_id": self.request_id,
                "operation": self.operation,
                "outcome": "not_executed",
                "may_have_been_applied": false,
                "error": "the request was cancelled before execution; no effect was applied"
            }),
        }
    }
}

/// Accept, validate and track up to [`MAX_PENDING`] connections.
///
/// The thread never outlives the owner: it returns as soon as `shutdown` is
/// set, refusing whatever is still `QUEUED`. It is also the owned driver that
/// propagates stop sources to the active request, so no additional polling
/// thread is created.
fn accept_loop(shared: AcceptShared) {
    let mut pending: Vec<Pending> = Vec::new();
    let mut next_marker_check = Instant::now();
    loop {
        if shared.shutdown.load(Ordering::Acquire) {
            for mut entry in pending.drain(..) {
                let _ = entry.stream.set_write_timeout(Some(SHUTDOWN_REPLY_TIMEOUT));
                if entry
                    .state
                    .compare_exchange(
                        QUEUED,
                        CANCELLED_BEFORE_EXECUTION,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok()
                {
                    // Only a proven still-queued request can be "not executed".
                    let value = entry.not_executed_reply();
                    let _ = write_reply(&mut entry.stream, value);
                } else if let Ok(value) = entry.reply.try_recv() {
                    let _ = write_reply(&mut entry.stream, value);
                } else {
                    // Claimed work is never restated as "nothing happened".
                    let value = entry.unknown_reply();
                    let _ = write_reply(&mut entry.stream, value);
                }
            }
            return;
        }
        // Admit new connections first, so the test-only scan gate can pause the
        // cancellation scan without also pausing admission. Admission is not
        // gated on the pending count: control requests must still be reachable
        // when ordinary work fills the bound, and over-capacity ordinary work
        // is refused by `admit` instead of being left in the backlog.
        match shared.listener.accept() {
            Ok((stream, _)) => {
                if let Some(entry) = admit(stream, &shared, &mut pending) {
                    pending.push(entry);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => {
                tracing::debug!(%error, "Managed PTY endpoint accept failed");
            }
        }
        #[cfg(test)]
        scan_gate::wait(shared.task_id.as_str());
        // A marker written by another process reaches this owned thread; a
        // guard in this process sets the same flags directly. Cancellation of
        // the active request is bounded by this poll plus one transport slice.
        if Instant::now() >= next_marker_check {
            if shared.owner.stop.load(Ordering::Acquire) || shared.cancel_marker.exists() {
                shared.owner.cancel.store(true, Ordering::Release);
            }
            next_marker_check = Instant::now() + MARKER_POLL;
        }
        if shared.owner.cancel.load(Ordering::Acquire) {
            for entry in &pending {
                entry.cancel.store(true, Ordering::Release);
            }
        }
        let mut index = 0;
        while index < pending.len() {
            match pending[index].reply.try_recv() {
                Ok(value) => {
                    let mut entry = pending.remove(index);
                    let _ = write_reply(&mut entry.stream, value);
                    continue;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    // The owner dropped the reply channel; the request itself is
                    // not undone, so nothing is claimed here.
                    pending.remove(index);
                    continue;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
            if let Some(grace) = pending[index].grace {
                if Instant::now() >= grace {
                    let mut entry = pending.remove(index);
                    let value = entry.unknown_reply();
                    let _ = write_reply(&mut entry.stream, value);
                } else {
                    index += 1;
                }
                continue;
            }
            if Instant::now() >= pending[index].deadline || peer_closed(&pending[index].stream) {
                // A claimed write must stop now, not at its own (possibly 60 s)
                // deadline. Setting the flag is safe for queued work too.
                pending[index].cancel.store(true, Ordering::Release);
                let transitioned = pending[index]
                    .state
                    .compare_exchange(
                        QUEUED,
                        CANCELLED_BEFORE_EXECUTION,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                    .is_ok();
                if transitioned {
                    let mut entry = pending.remove(index);
                    let value = entry.not_executed_reply();
                    let _ = write_reply(&mut entry.stream, value);
                } else {
                    // Already claimed (or finished): wait for the real result
                    // and never pretend that nothing happened.
                    pending[index].grace = Some(Instant::now() + EXECUTION_GRACE);
                    index += 1;
                }
                continue;
            }
            index += 1;
        }
        std::thread::sleep(ACCEPT_TICK);
    }
}

/// Read, validate and either queue a request or apply a Stop. `None` means the
/// connection is done.
///
/// A Stop is claimed here, on the accept thread, after all authorization and
/// the deadline check and before any side effect. It is therefore impossible
/// for an accepted Stop to be reported later as not executed, and impossible
/// for an expired or unauthorized Stop to cancel anything.
fn admit(
    mut stream: UnixStream,
    shared: &AcceptShared,
    pending: &mut Vec<Pending>,
) -> Option<Pending> {
    let _ = stream.set_read_timeout(Some(SOCKET_IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(SOCKET_IO_TIMEOUT));
    let frame = match read_frame(&mut stream, MAX_FRAME_BYTES) {
        Ok(frame) => frame,
        Err(error) => {
            // The owner keeps running; nothing was queued.
            let _ = write_reply(
                &mut stream,
                json!({"ok":false,"error":format!("{error:#}")}),
            );
            return None;
        }
    };
    let request: Value = match serde_json::from_slice(&frame) {
        Ok(request) => request,
        Err(error) => {
            let _ = write_reply(
                &mut stream,
                json!({"ok":false,"error":format!("invalid request JSON: {error}")}),
            );
            return None;
        }
    };
    if request["version"].as_u64() != Some(1) {
        let _ = write_reply(
            &mut stream,
            json!({"ok":false,"error":"unsupported protocol version"}),
        );
        return None;
    }
    let token = request["token"].as_str().unwrap_or_default();
    if !constant_time_eq(token.as_bytes(), shared.token.as_bytes()) {
        let _ = write_reply(
            &mut stream,
            json!({"ok":false,"error":"the request token does not match this terminal generation"}),
        );
        return None;
    }
    // The server checks the envelope itself: caller-side checks are defence in
    // depth, not the authorization.
    if request["generation"].as_str() != Some(shared.generation.as_str()) {
        let _ = write_reply(
            &mut stream,
            json!({"ok":false,"error":"the request generation does not match this terminal"}),
        );
        return None;
    }
    if request["task_id"].as_str() != Some(shared.task_id.as_str()) {
        let _ = write_reply(
            &mut stream,
            json!({"ok":false,"error":"the request task does not belong to this terminal"}),
        );
        return None;
    }
    if request["session"].as_str() != Some(shared.session.as_str()) {
        let _ = write_reply(
            &mut stream,
            json!({"ok":false,"error":"the request session does not own this terminal"}),
        );
        return None;
    }
    let request_id = request["request_id"].as_str().unwrap_or_default();
    if request_id.is_empty() || request_id.len() > 64 {
        let _ = write_reply(
            &mut stream,
            json!({"ok":false,"error":"request_id must be 1..=64 characters"}),
        );
        return None;
    }
    let request_id = request_id.to_owned();
    let timeout = request_timeout(&request);
    let deadline = Instant::now() + timeout;
    #[cfg(test)]
    admit_gate::wait(shared.task_id.as_str(), &request_id);
    if request["operation"].as_str() == Some("stop") {
        if Instant::now() >= deadline {
            let _ = write_reply(
                &mut stream,
                not_executed(
                    &request_id,
                    "stop",
                    "the caller's deadline passed before the stop was claimed; no stop was requested",
                ),
            );
            return None;
        }
        // Claim before effect. The request cell is local to this admitted
        // connection, but the same invariant as the owner path applies: only a
        // claimed Stop may change the session, and the reply written here is
        // the final outcome (there is no later command to reclassify).
        let state = AtomicU8::new(QUEUED);
        if state
            .compare_exchange(QUEUED, CLAIMED, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            let _ = write_reply(
                &mut stream,
                not_executed(
                    &request_id,
                    "stop",
                    "the stop could not be claimed; no stop was requested",
                ),
            );
            return None;
        }
        shared.owner.cancel.store(true, Ordering::Release);
        shared.owner.stop.store(true, Ordering::Release);
        for waiting in pending.iter() {
            waiting.cancel.store(true, Ordering::Release);
        }
        let _ = write_reply(&mut stream, stop_accepted(&request_id));
        return None;
    }
    // Control connectivity is preserved at the bound: the Stop branch above is
    // handled regardless of capacity. Ordinary work beyond it is refused
    // explicitly here, before any queue entry exists, so the pending set and
    // its memory stay bounded instead of a new connection waiting in the
    // backlog.
    if pending.len() >= MAX_PENDING {
        let operation = request["operation"].as_str().unwrap_or("request");
        let _ = write_reply(
            &mut stream,
            not_executed(
                &request_id,
                operation,
                "the managed PTY owner already has the maximum number of requests in flight; this request was refused before execution",
            ),
        );
        return None;
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let state = Arc::new(AtomicU8::new(QUEUED));
    let (reply, receiver) = mpsc::sync_channel(1);
    let (command, operation) = match request["operation"].as_str() {
        Some("write") => {
            let Some(data) = request["data"].as_str() else {
                let _ = write_reply(
                    &mut stream,
                    json!({"ok":false,"request_id":request_id,"error":"write needs a string data field"}),
                );
                return None;
            };
            if data.len() > MAX_DATA_BYTES {
                let _ = write_reply(
                    &mut stream,
                    json!({"ok":false,"request_id":request_id,
                           "error":format!("data is {} bytes; at most {MAX_DATA_BYTES} are accepted", data.len())}),
                );
                return None;
            }
            (
                Command::Write {
                    request_id: request_id.clone(),
                    data: data.as_bytes().to_vec(),
                    then_eof: request["then_eof"].as_bool().unwrap_or(false),
                    deadline,
                    cancel: cancel.clone(),
                    state: state.clone(),
                    reply,
                },
                "write",
            )
        }
        Some("eof") => (
            Command::Eof {
                request_id: request_id.clone(),
                deadline,
                cancel: cancel.clone(),
                state: state.clone(),
                reply,
            },
            "eof",
        ),
        Some("resize") => {
            let rows = request["rows"].as_u64().unwrap_or(0);
            let cols = request["cols"].as_u64().unwrap_or(0);
            if !(1..=u64::from(pty_transport::MAX_ROWS)).contains(&rows)
                || !(1..=u64::from(pty_transport::MAX_COLS)).contains(&cols)
            {
                let _ = write_reply(
                    &mut stream,
                    json!({"ok":false,"request_id":request_id,
                           "error":format!("rows must be 1..={} and cols 1..={}",
                                           pty_transport::MAX_ROWS, pty_transport::MAX_COLS)}),
                );
                return None;
            }
            (
                Command::Resize {
                    request_id: request_id.clone(),
                    rows: rows as u16,
                    cols: cols as u16,
                    deadline,
                    state: state.clone(),
                    reply,
                },
                "resize",
            )
        }
        other => {
            let _ = write_reply(
                &mut stream,
                json!({"ok":false,"request_id":request_id,
                       "error":format!("unsupported operation {other:?}")}),
            );
            return None;
        }
    };
    match shared.sender.try_send(command) {
        Ok(()) => {
            #[cfg(test)]
            claim_probe::queued(&request_id);
            Some(Pending {
                stream,
                reply: receiver,
                state,
                cancel,
                deadline,
                grace: None,
                request_id,
                operation,
            })
        }
        Err(mpsc::TrySendError::Full(_)) => {
            let _ = write_reply(
                &mut stream,
                not_executed(
                    &request_id,
                    operation,
                    "the managed PTY owner is busy; the request was refused before execution",
                ),
            );
            None
        }
        Err(mpsc::TrySendError::Disconnected(_)) => {
            let _ = write_reply(
                &mut stream,
                not_executed(
                    &request_id,
                    operation,
                    "the managed PTY owner is shutting down; nothing was executed",
                ),
            );
            None
        }
    }
}

/// Whether the caller closed its side. One request per connection, so any
/// readable byte after the frame is also treated as "no longer waiting".
fn peer_closed(stream: &UnixStream) -> bool {
    let raw = stream.as_raw_fd();
    // SAFETY: `stream` outlives the borrow.
    let borrowed = unsafe { BorrowedFd::borrow_raw(raw) };
    let mut fds = [rustix::event::PollFd::new(
        &borrowed,
        rustix::event::PollFlags::IN
            | rustix::event::PollFlags::HUP
            | rustix::event::PollFlags::ERR,
    )];
    let timeout = rustix::event::Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    match rustix::event::poll(&mut fds, Some(&timeout)) {
        Ok(0) => false,
        Ok(_) => {
            let mut byte = [0u8; 1];
            let _ = stream.set_nonblocking(true);
            let closed = match (&*stream).read(&mut byte) {
                Ok(0) => true,
                Ok(_) => true,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => false,
                Err(_) => true,
            };
            let _ = stream.set_nonblocking(false);
            closed
        }
        Err(_) => false,
    }
}

fn request_timeout(request: &Value) -> Duration {
    let millis = request["timeout_ms"]
        .as_u64()
        .filter(|millis| (1..=MAX_CALL_MS).contains(millis))
        .unwrap_or(DEFAULT_CALL_MS);
    Duration::from_millis(millis)
}

fn write_reply(stream: &mut UnixStream, value: Value) -> std::io::Result<()> {
    write_frame(stream, value.to_string().as_bytes())
}

fn write_frame(stream: &mut UnixStream, body: &[u8]) -> std::io::Result<()> {
    stream.write_all(&(body.len() as u32).to_be_bytes())?;
    stream.write_all(body)
}

fn read_frame(stream: &mut UnixStream, max: usize) -> Result<Vec<u8>> {
    let mut length = [0u8; 4];
    stream
        .read_exact(&mut length)
        .context("Reading the request frame length")?;
    let length = u32::from_be_bytes(length) as usize;
    ensure!(
        length > 0 && length <= max,
        "frame is {length} bytes; the accepted range is 1..={max}"
    );
    let mut body = vec![0u8; length];
    stream
        .read_exact(&mut body)
        .context("Reading the request frame body")?;
    Ok(body)
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in left.iter().zip(right) {
        diff |= a ^ b;
    }
    diff == 0
}

fn random_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    let mut file = std::fs::File::open("/dev/urandom").context("Opening /dev/urandom")?;
    file.read_exact(&mut bytes)
        .context("Reading endpoint randomness")?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("Creating {}", path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("Writing {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("Restricting {}", path.display()))?;
    Ok(())
}

/// Publish one small metadata file through a private temporary file and an
/// atomic rename, so a failed write never leaves a half-written result that a
/// reader could mistake for a terminal state.
///
/// `write_all`/`flush`/`rename` completion is filesystem acceptance, not
/// `fsync` durability: no `fsync` is issued on either the file or its parent.
fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("terminal-state.json");
    let staging = parent.join(format!(".{name}.new-{}", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&staging)
            .with_context(|| format!("Creating {}", staging.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("Writing {}", staging.display()))?;
        file.flush()
            .with_context(|| format!("Flushing {}", staging.display()))?;
        std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("Restricting {}", staging.display()))?;
        std::fs::rename(&staging, path)
            .with_context(|| format!("Publishing {} over {}", staging.display(), path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&staging);
    }
    result
}

fn endpoint_paths() -> (PathBuf, PathBuf, String) {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute() && path.is_dir())
        .unwrap_or_else(std::env::temp_dir);
    let uid = std::fs::metadata("/proc/self")
        .map(|metadata| metadata.uid())
        .unwrap_or(0);
    let directory = base.join(format!("dcl-pty-{uid}"));
    let generation = uuid::Uuid::new_v4().to_string();
    let socket = directory.join(format!("pty-{generation}.sock"));
    (directory, socket, generation)
}

fn remaining_budget(deadline: Instant) -> Duration {
    deadline
        .saturating_duration_since(Instant::now())
        .max(Duration::from_millis(1))
}

/// One caller's request: `data`/`eof` write to the terminal, `resize` changes
/// its size, `stop` asks the owner to terminate the session.
pub(crate) fn execute(
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
    let request = request_operation(input)?;
    if let Request::Write { data, .. } = &request {
        ensure!(
            data.len() <= MAX_DATA_BYTES,
            "data is {} bytes; one call accepts at most {MAX_DATA_BYTES} bytes",
            data.len()
        );
    }
    // Ownership is the caller's own session: an ID alone is never authorization.
    let task = super::tasks::read_for_session(&task_id, root, session)?;
    ensure!(
        task["metadata"]["managed_shell"] == true,
        "Task {task_id} is not a managed shell task"
    );
    ensure!(
        task["metadata"]["interaction"].as_str() == Some(INTERACTION),
        "Task {task_id} is not an internal managed PTY task; this control API is not exposed as a tool interaction"
    );
    let stored = task["status"].as_str().unwrap_or("unknown");
    ensure!(
        stored == "running",
        "Task {task_id} is not running (status {stored}); its terminal is unavailable"
    );
    let directory = super::background_shell::task_directory(root, session, &task)?;
    // The exact lease/pidfd rules: a missing, free, legacy or unreadable owner
    // never authorizes a connection.
    let owner = super::background_shell::OwnerProbe::open(&directory)?;
    let state = owner.state();
    ensure!(state.is_held(), "{}", state.refusal(&task_id));
    let status = super::background_shell::task_status(root, session, &task)?;
    ensure!(
        status == "running",
        "Task {task_id} is running but its owner stopped reporting a heartbeat (status {status}); its terminal is unavailable"
    );
    let endpoint = read_endpoint(&directory, &task_id, session)?;
    #[cfg(test)]
    swap_probe::reached();
    if cancelled.is_some_and(|flag| flag.load(Ordering::Acquire)) {
        // Nothing was connected and this protocol has no deferred queue.
        return Ok(json!({
            "task_id": task_id,
            "operation": request.name(),
            "input_mode": INTERACTION,
            "bytes_written": 0,
            "cancelled": true,
            "queued": false,
            "may_have_been_written": false,
            "is_error": true,
            "message": "The call was cancelled before connecting; nothing was sent and nothing is queued"
        }));
    }
    // One absolute deadline for the whole call. Connect and framing are charged
    // to it, and only the remainder is transmitted to the owner.
    let timeout = call_timeout(input);
    let deadline = Instant::now() + timeout;
    let request_id = uuid::Uuid::new_v4().to_string();
    let mut stream = connect_bounded(&endpoint.socket, deadline, cancelled)
        .with_context(|| format!("Connecting to the managed terminal of task {task_id}"))?;
    // The connected peer and the path are re-verified *after* connecting: a
    // stat before the connect cannot prove what the connection reached.
    let peer = rustix::net::sockopt::socket_peercred(&stream)
        .context("Reading the managed terminal peer credentials")?;
    ensure!(
        peer.uid.as_raw() == my_uid(),
        "Task {task_id} connected to a terminal owned by another user"
    );
    ensure!(
        peer.pid.as_raw_pid() as u32 == endpoint.owner_pid,
        "Task {task_id} connected to a terminal served by another process"
    );
    let after = rustix::fs::stat(&endpoint.socket)
        .with_context(|| format!("Re-checking the endpoint of task {task_id}"))?;
    ensure!(
        after.st_dev == endpoint.dev && after.st_ino == endpoint.ino,
        "Task {task_id} endpoint was replaced while connecting; no request was sent"
    );
    let budget = remaining_budget(deadline);
    stream
        .set_write_timeout(Some(budget))
        .context("Bounding the request write")?;
    stream
        .set_read_timeout(Some(budget))
        .context("Bounding the reply read")?;
    let body = request.encode(
        &request_id,
        &endpoint.token,
        &endpoint.generation,
        &task_id,
        session,
        budget,
    );
    if let Err(error) = write_frame(&mut stream, body.as_bytes()) {
        bail!(
            "Task {task_id} did not accept the request: {error}. Nothing was written to the terminal."
        );
    }
    let frame = match read_frame(&mut stream, MAX_REPLY_BYTES) {
        Ok(frame) => frame,
        Err(error) => {
            // The request was fully delivered; the terminal may have taken the
            // bytes. Report that and never retry: there is no dedup record.
            return Ok(json!({
                "task_id": task_id,
                "operation": request.name(),
                "input_mode": INTERACTION,
                "request_id": request_id,
                "outcome": "unknown",
                "may_have_been_written": true,
                "is_error": true,
                "error": format!("the reply from the managed terminal was lost after the request was sent: {error:#}"),
                "message": "Do not repeat this call blindly: the terminal may already have accepted the input, and no dedup record exists."
            }));
        }
    };
    let reply: Value = serde_json::from_slice(&frame)
        .context("The managed terminal returned a malformed reply")?;
    ensure!(
        reply["request_id"].as_str() == Some(request_id.as_str()),
        "The managed terminal replied to a different request"
    );
    ensure!(
        reply["generation"].as_str().is_none()
            || reply["generation"].as_str() == Some(endpoint.generation.as_str()),
        "The managed terminal replied for a different terminal generation"
    );
    let mut value = reply;
    value["task_id"] = json!(task_id);
    value["input_mode"] = json!(INTERACTION);
    value["merged_output"] = json!(true);
    if value["ok"] == false {
        value["is_error"] = json!(true);
    }
    Ok(value)
}

enum Request {
    Write { data: String, eof: bool },
    Resize { rows: u16, cols: u16 },
    Stop,
}

impl Request {
    fn name(&self) -> &'static str {
        match self {
            Self::Write { .. } => "write",
            Self::Resize { .. } => "resize",
            Self::Stop => "stop",
        }
    }

    fn encode(
        &self,
        request_id: &str,
        token: &str,
        generation: &str,
        task_id: &str,
        session: &str,
        budget: Duration,
    ) -> String {
        let timeout_ms = budget.as_millis().clamp(1, u128::from(MAX_CALL_MS)) as u64;
        let envelope = |operation: &str| {
            json!({
                "version": 1,
                "token": token,
                "generation": generation,
                "task_id": task_id,
                "session": session,
                "request_id": request_id,
                "operation": operation
            })
        };
        match self {
            Self::Write { data, eof } => {
                if *eof && data.is_empty() {
                    let mut value = envelope("eof");
                    value["timeout_ms"] = json!(timeout_ms);
                    value.to_string()
                } else {
                    let mut value = envelope("write");
                    value["data"] = json!(data);
                    value["timeout_ms"] = json!(timeout_ms);
                    value["then_eof"] = json!(eof);
                    value.to_string()
                }
            }
            Self::Resize { rows, cols } => {
                let mut value = envelope("resize");
                value["rows"] = json!(rows);
                value["cols"] = json!(cols);
                value["timeout_ms"] = json!(timeout_ms);
                value.to_string()
            }
            Self::Stop => {
                let mut value = envelope("stop");
                value["timeout_ms"] = json!(timeout_ms);
                value.to_string()
            }
        }
    }
}

fn request_operation(input: &Value) -> Result<Request> {
    if input.get("stop").and_then(Value::as_bool) == Some(true) {
        return Ok(Request::Stop);
    }
    if let Some(resize) = input.get("resize").filter(|value| !value.is_null()) {
        let rows = resize["rows"].as_u64().unwrap_or(0);
        let cols = resize["cols"].as_u64().unwrap_or(0);
        ensure!(
            (1..=u64::from(pty_transport::MAX_ROWS)).contains(&rows)
                && (1..=u64::from(pty_transport::MAX_COLS)).contains(&cols),
            "resize rows must be 1..={} and cols 1..={}",
            pty_transport::MAX_ROWS,
            pty_transport::MAX_COLS
        );
        return Ok(Request::Resize {
            rows: rows as u16,
            cols: cols as u16,
        });
    }
    let data = match input.get("data") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(text)) => text.clone(),
        Some(_) => bail!("data must be a string"),
    };
    let eof = match input.get("eof") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(_) => bail!("eof must be a boolean"),
    };
    ensure!(
        !data.is_empty() || eof,
        "the call needs data and/or eof: true"
    );
    Ok(Request::Write { data, eof })
}

fn call_timeout(input: &Value) -> Duration {
    let millis = input["timeout"]
        .as_u64()
        .filter(|millis| (1..=MAX_CALL_MS).contains(millis))
        .unwrap_or(DEFAULT_CALL_MS);
    Duration::from_millis(millis)
}

struct Endpoint {
    socket: PathBuf,
    token: String,
    generation: String,
    /// The process that bound the socket; checked against `SO_PEERCRED` after
    /// connecting.
    owner_pid: u32,
    /// The exact bound socket, re-checked after connecting.
    dev: u64,
    ino: u64,
}

fn my_uid() -> u32 {
    std::fs::metadata("/proc/self")
        .map(|metadata| metadata.uid())
        .unwrap_or(0)
}

/// Read and validate the live endpoint descriptor.
///
/// The recorded socket identity (`st_dev`/`st_ino`) binds the path to the socket
/// the owner actually bound: a replacement in a same-uid directory has a
/// different inode and is refused before connecting.
fn read_endpoint(directory: &Path, task_id: &str, session: &str) -> Result<Endpoint> {
    let path = directory.join("terminal.json");
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("Reading the terminal descriptor for task {task_id}"))?;
    let value: Value = serde_json::from_str(&text)
        .with_context(|| format!("Parsing the terminal descriptor for task {task_id}"))?;
    ensure!(
        value["version"].as_u64() == Some(1),
        "Task {task_id} has an unsupported terminal descriptor version"
    );
    ensure!(
        value["task_id"].as_str() == Some(task_id),
        "Task {task_id} records a descriptor for another task"
    );
    ensure!(
        value["session"].as_str() == Some(session),
        "Task {task_id} records a descriptor for another session"
    );
    let socket = PathBuf::from(
        value["endpoint"]
            .as_str()
            .filter(|path| !path.is_empty())
            .context("the terminal descriptor has no endpoint")?,
    );
    let token = value["token"]
        .as_str()
        .filter(|token| !token.is_empty())
        .context("the terminal descriptor has no token")?
        .to_owned();
    let generation = value["generation"]
        .as_str()
        .filter(|generation| !generation.is_empty())
        .context("the terminal descriptor has no generation")?
        .to_owned();
    let recorded_dev = value["socket_dev"].as_u64().unwrap_or(u64::MAX);
    let owner_pid = value["owner_pid"]
        .as_u64()
        .and_then(|pid| u32::try_from(pid).ok())
        .filter(|pid| *pid != 0)
        .context("the terminal descriptor has no owner process")?;
    let recorded_ino = value["socket_ino"].as_u64().unwrap_or(u64::MAX);
    ensure!(
        socket
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == format!("pty-{generation}.sock")),
        "Task {task_id} records an endpoint that does not match its generation"
    );
    let parent = socket
        .parent()
        .context("The terminal endpoint has no parent directory")?;
    let parent_metadata = std::fs::symlink_metadata(parent)
        .with_context(|| format!("Inspecting {}", parent.display()))?;
    ensure!(
        parent_metadata.is_dir() && !parent_metadata.file_type().is_symlink(),
        "Task {task_id} uses an endpoint directory that is not a real directory"
    );
    let my_uid = my_uid();
    let directory_stat =
        rustix::fs::stat(parent).with_context(|| format!("Inspecting {}", parent.display()))?;
    ensure!(
        directory_stat.st_uid == my_uid,
        "Task {task_id} uses an endpoint directory owned by another user"
    );
    ensure!(
        directory_stat.st_mode & 0o077 == 0,
        "Task {task_id} uses an endpoint directory reachable by other users"
    );
    let metadata = std::fs::symlink_metadata(&socket)
        .with_context(|| format!("Inspecting the endpoint of task {task_id}"))?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "Task {task_id} records a symlinked endpoint"
    );
    let socket_stat = rustix::fs::stat(&socket)
        .with_context(|| format!("Inspecting the endpoint of task {task_id}"))?;
    ensure!(
        socket_stat.st_mode & 0o170000 == 0o140000,
        "Task {task_id} records an endpoint that is not a socket"
    );
    ensure!(
        socket_stat.st_uid == my_uid && socket_stat.st_mode & 0o077 == 0,
        "Task {task_id} records an endpoint other users can reach"
    );
    ensure!(
        socket_stat.st_dev == recorded_dev && socket_stat.st_ino == recorded_ino,
        "Task {task_id} records an endpoint that has been replaced: its identity does not match the bound socket"
    );
    Ok(Endpoint {
        socket,
        token,
        generation,
        owner_pid,
        dev: recorded_dev,
        ino: recorded_ino,
    })
}

/// Connect with an owned, bounded, cancellable attempt.
///
/// The socket is created nonblocking, so `connect(2)` returns immediately and a
/// saturated backlog cannot block a thread. Readiness and the pending socket
/// error are polled in short slices against the caller's absolute deadline and
/// cancellation flag. No helper thread exists to outlive a timed-out attempt.
fn connect_bounded(
    path: &Path,
    deadline: Instant,
    cancelled: Option<&AtomicBool>,
) -> Result<UnixStream> {
    let address = rustix::net::SocketAddrUnix::new(path)
        .with_context(|| format!("Encoding the endpoint address {}", path.display()))?;
    let socket = rustix::net::socket_with(
        rustix::net::AddressFamily::UNIX,
        rustix::net::SocketType::STREAM,
        // The descriptor is owned here; closing it is what releases a failed or
        // abandoned attempt.
        rustix::net::SocketFlags::CLOEXEC | rustix::net::SocketFlags::NONBLOCK,
        None,
    )
    .context("Creating the endpoint socket")?;
    // `EINPROGRESS` means the attempt is running; `EAGAIN` on AF_UNIX means the
    // accept backlog is full and the attempt was not started, so it must be
    // retried rather than polled (polling would report a socket that never
    // connected as writable).
    let mut pending = false;
    let mut connected = false;
    match rustix::net::connect(&socket, &address) {
        // Connected immediately (a listening server with queue room).
        Ok(()) => connected = true,
        Err(rustix::io::Errno::INPROGRESS) => pending = true,
        Err(rustix::io::Errno::AGAIN) => {}
        Err(error) => return Err(error).context("Connecting to the managed terminal"),
    }
    while !connected {
        if cancelled.is_some_and(|flag| flag.load(Ordering::Acquire)) {
            bail!("the connection attempt was cancelled before it completed");
        }
        let now = Instant::now();
        if now >= deadline {
            bail!("the managed terminal did not accept a connection before the deadline");
        }
        if !pending {
            // The backlog was full: retry the connect call itself.
            match rustix::net::connect(&socket, &address) {
                Ok(()) => connected = true,
                Err(rustix::io::Errno::INPROGRESS) => pending = true,
                Err(rustix::io::Errno::AGAIN) => {
                    std::thread::sleep(CONNECT_RETRY);
                }
                Err(error) => return Err(error).context("Connecting to the managed terminal"),
            }
            continue;
        }
        let slice = deadline.saturating_duration_since(now).min(CONNECT_SLICE);
        let mut fds = [rustix::event::PollFd::new(
            &socket,
            rustix::event::PollFlags::OUT,
        )];
        let timeout = rustix::event::Timespec {
            tv_sec: slice.as_secs() as _,
            tv_nsec: slice.subsec_nanos() as _,
        };
        match rustix::event::poll(&mut fds, Some(&timeout)) {
            Ok(0) => continue,
            Ok(_) => match rustix::net::sockopt::socket_error(&socket) {
                Ok(Ok(())) => connected = true,
                Ok(Err(error)) => return Err(error).context("Connecting to the managed terminal"),
                Err(error) => return Err(error).context("Reading the connection status"),
            },
            Err(rustix::io::Errno::INTR) => continue,
            Err(error) => return Err(error).context("Waiting for the managed terminal"),
        }
    }
    let stream = UnixStream::from(socket);
    stream
        .set_nonblocking(false)
        .context("Restoring blocking endpoint behaviour")?;
    Ok(stream)
}

/// Test-only observation point between endpoint validation and connecting, so a
/// test can swap the path inside that window deterministically.
#[cfg(test)]
pub(crate) mod swap_probe {
    use std::cell::RefCell;

    thread_local! {
        static HOOK: RefCell<Option<Box<dyn FnMut()>>> = const { RefCell::new(None) };
    }

    pub(crate) fn install(hook: Box<dyn FnMut()>) {
        HOOK.with(|slot| *slot.borrow_mut() = Some(hook));
    }

    pub(crate) fn clear() {
        HOOK.with(|slot| *slot.borrow_mut() = None);
    }

    pub(super) fn reached() {
        HOOK.with(|slot| {
            if let Some(hook) = slot.borrow_mut().as_mut() {
                hook();
            }
        });
    }
}

/// Test-only claim/finish notifications from the owner thread.
///
/// A scheduler sleep is not proof that the owner claimed a request, so tests
/// wait on this instead. Keyed by the request id; entries are removed when a
/// waiting test observes completion.
#[cfg(test)]
pub(crate) mod claim_probe {
    use std::{
        collections::HashMap,
        sync::{Condvar, Mutex, OnceLock},
    };

    #[derive(Default, Clone, Copy)]
    struct Entry {
        queued: bool,
        claimed: bool,
        finished: bool,
        accepted: usize,
    }

    fn state() -> &'static (Mutex<HashMap<String, Entry>>, Condvar) {
        static STATE: OnceLock<(Mutex<HashMap<String, Entry>>, Condvar)> = OnceLock::new();
        STATE.get_or_init(|| (Mutex::new(HashMap::new()), Condvar::new()))
    }

    fn update(id: &str, change: impl FnOnce(&mut Entry)) {
        let (lock, signal) = state();
        let mut map = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        change(map.entry(id.to_owned()).or_default());
        signal.notify_all();
    }

    pub(crate) fn queued(id: &str) {
        update(id, |entry| entry.queued = true);
    }

    pub(crate) fn claimed(id: &str) {
        update(id, |entry| entry.claimed = true);
    }

    pub(crate) fn accepted(id: &str, count: usize) {
        update(id, |entry| {
            entry.accepted = entry.accepted.saturating_add(count)
        });
    }

    pub(crate) fn finished(id: &str) {
        update(id, |entry| entry.finished = true);
    }

    pub(crate) fn accepted_bytes(id: &str) -> usize {
        let (lock, _) = state();
        let map = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        map.get(id).map(|entry| entry.accepted).unwrap_or(0)
    }

    pub(crate) fn is_finished(id: &str) -> bool {
        let (lock, _) = state();
        let map = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        map.get(id).is_some_and(|entry| entry.finished)
    }

    pub(crate) fn wait_queued(id: &str, deadline: std::time::Instant) -> bool {
        wait(id, deadline, |entry| entry.queued)
    }

    pub(crate) fn wait_claimed(id: &str, deadline: std::time::Instant) -> bool {
        wait(id, deadline, |entry| entry.claimed)
    }

    pub(crate) fn wait_partial(id: &str, deadline: std::time::Instant) -> bool {
        wait(id, deadline, |entry| entry.accepted > 0)
    }

    pub(crate) fn wait_finished(id: &str, deadline: std::time::Instant) -> bool {
        wait(id, deadline, |entry| entry.finished)
    }

    fn wait(id: &str, deadline: std::time::Instant, check: impl Fn(&Entry) -> bool) -> bool {
        let (lock, signal) = state();
        let mut map = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if let Some(entry) = map.get(id) {
                if check(entry) {
                    return true;
                }
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return false;
            }
            let (guard, _) = signal
                .wait_timeout(map, deadline - now)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            map = guard;
        }
    }
}

/// Test-only barrier for the owner between popping a command and claiming it.
/// Keyed by request id, so unrelated commands are unaffected.
#[cfg(test)]
pub(crate) mod serve_gate {
    use std::sync::{Condvar, Mutex, OnceLock};

    #[derive(Default)]
    struct State {
        armed: Option<String>,
        entered: bool,
        open: bool,
    }

    fn state() -> &'static (Mutex<State>, Condvar) {
        static STATE: OnceLock<(Mutex<State>, Condvar)> = OnceLock::new();
        STATE.get_or_init(|| (Mutex::new(State::default()), Condvar::new()))
    }

    pub(crate) fn arm(id: &str) {
        let (lock, signal) = state();
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.armed = Some(id.to_owned());
        state.entered = false;
        state.open = false;
        signal.notify_all();
    }

    pub(crate) fn clear() {
        let (lock, signal) = state();
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.armed = None;
        state.open = true;
        signal.notify_all();
    }

    pub(crate) fn entered(deadline: std::time::Instant) -> bool {
        let (lock, signal) = state();
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if state.entered {
                return true;
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return false;
            }
            let (guard, _) = signal
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = guard;
        }
    }

    pub(crate) fn open() {
        let (lock, signal) = state();
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.open = true;
        signal.notify_all();
    }

    pub(super) fn wait(id: &str) {
        let (lock, signal) = state();
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.armed.as_deref() != Some(id) || state.open {
            return;
        }
        state.entered = true;
        signal.notify_all();
        while !state.open {
            state = signal
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }
}

/// Test-only barrier for the accept thread's cancellation scan. Keyed by task
/// id, so only the intended terminal is paused.
#[cfg(test)]
pub(crate) mod scan_gate {
    use std::sync::{Condvar, Mutex, OnceLock};

    #[derive(Default)]
    struct State {
        armed: Option<String>,
        entered: bool,
        open: bool,
    }

    fn state() -> &'static (Mutex<State>, Condvar) {
        static STATE: OnceLock<(Mutex<State>, Condvar)> = OnceLock::new();
        STATE.get_or_init(|| (Mutex::new(State::default()), Condvar::new()))
    }

    pub(crate) fn arm(task_id: &str) {
        let (lock, signal) = state();
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.armed = Some(task_id.to_owned());
        state.entered = false;
        state.open = false;
        signal.notify_all();
    }

    pub(crate) fn clear() {
        let (lock, signal) = state();
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.armed = None;
        state.open = true;
        signal.notify_all();
    }

    pub(crate) fn entered(deadline: std::time::Instant) -> bool {
        let (lock, signal) = state();
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if state.entered {
                return true;
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return false;
            }
            let (guard, _) = signal
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = guard;
        }
    }

    pub(crate) fn open() {
        let (lock, signal) = state();
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.open = true;
        signal.notify_all();
    }

    pub(super) fn wait(task_id: &str) {
        let (lock, signal) = state();
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.armed.as_deref() != Some(task_id) || state.open {
            return;
        }
        state.entered = true;
        signal.notify_all();
        while !state.open {
            state = signal
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }
}

/// Test-only barrier between computing a request's deadline and claiming it in
/// `admit`. Keyed by task and request, so only the intended Stop pauses.
#[cfg(test)]
pub(crate) mod admit_gate {
    use std::sync::{Condvar, Mutex, OnceLock};

    #[derive(Default)]
    struct State {
        task: Option<String>,
        request: Option<String>,
        entered: bool,
        open: bool,
    }

    fn state() -> &'static (Mutex<State>, Condvar) {
        static STATE: OnceLock<(Mutex<State>, Condvar)> = OnceLock::new();
        STATE.get_or_init(|| (Mutex::new(State::default()), Condvar::new()))
    }

    pub(crate) fn arm(task_id: &str, request_id: &str) {
        let (lock, signal) = state();
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.task = Some(task_id.to_owned());
        state.request = Some(request_id.to_owned());
        state.entered = false;
        state.open = false;
        signal.notify_all();
    }

    pub(crate) fn clear() {
        let (lock, signal) = state();
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.task = None;
        state.request = None;
        state.open = true;
        signal.notify_all();
    }

    pub(crate) fn entered(deadline: std::time::Instant) -> bool {
        let (lock, signal) = state();
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if state.entered {
                return true;
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return false;
            }
            let (guard, _) = signal
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = guard;
        }
    }

    pub(crate) fn open() {
        let (lock, signal) = state();
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.open = true;
        signal.notify_all();
    }

    pub(super) fn wait(task_id: &str, request_id: &str) {
        let (lock, signal) = state();
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.task.as_deref() != Some(task_id)
            || state.request.as_deref() != Some(request_id)
            || state.open
        {
            return;
        }
        state.entered = true;
        signal.notify_all();
        while !state.open {
            state = signal
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }
}

/// Test-only barrier immediately before `transport.stop()`, so a test can let
/// the real child fill the terminal output queue and then prove the final drain
/// reads those queued bytes.
#[cfg(test)]
pub(crate) mod pre_stop_gate {
    use std::sync::{Condvar, Mutex, OnceLock};

    #[derive(Default)]
    struct State {
        armed: Option<String>,
        entered: bool,
        open: bool,
    }

    fn state() -> &'static (Mutex<State>, Condvar) {
        static STATE: OnceLock<(Mutex<State>, Condvar)> = OnceLock::new();
        STATE.get_or_init(|| (Mutex::new(State::default()), Condvar::new()))
    }

    pub(crate) fn arm(task_id: &str) {
        let (lock, signal) = state();
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.armed = Some(task_id.to_owned());
        state.entered = false;
        state.open = false;
        signal.notify_all();
    }

    pub(crate) fn clear() {
        let (lock, signal) = state();
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.armed = None;
        state.open = true;
        signal.notify_all();
    }

    pub(crate) fn entered(deadline: std::time::Instant) -> bool {
        let (lock, signal) = state();
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if state.entered {
                return true;
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return false;
            }
            let (guard, _) = signal
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = guard;
        }
    }

    pub(crate) fn open() {
        let (lock, signal) = state();
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        state.open = true;
        signal.notify_all();
    }

    pub(super) fn wait(task_id: &str) {
        let (lock, signal) = state();
        let mut state = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.armed.as_deref() != Some(task_id) || state.open {
            return;
        }
        state.entered = true;
        signal.notify_all();
        while !state.open {
            state = signal
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }
}

/// Test-only observation point inside `start`, on the caller's thread, after
/// registration and before the owner thread is spawned. Thread-local, so it
/// cannot affect another test's start.
#[cfg(test)]
pub(crate) mod startup_probe {
    use std::cell::RefCell;

    thread_local! {
        static HOOK: RefCell<Option<Box<dyn Fn(&str)>>> = const { RefCell::new(None) };
    }

    pub(crate) fn install(hook: Box<dyn Fn(&str)>) {
        HOOK.with(|slot| *slot.borrow_mut() = Some(hook));
    }

    pub(crate) fn clear() {
        HOOK.with(|slot| *slot.borrow_mut() = None);
    }

    pub(super) fn registered(id: &str) {
        HOOK.with(|slot| {
            if let Some(hook) = slot.borrow().as_ref() {
                hook(id);
            }
        });
    }
}

/// Test-only per-task registration view, mirroring the process-wide
/// `has_active()` counter for the exact task a test started.
#[cfg(test)]
pub(crate) mod registration_probe {
    use std::{
        collections::HashSet,
        sync::{Mutex, OnceLock},
    };

    fn set() -> &'static Mutex<HashSet<String>> {
        static SET: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
        SET.get_or_init(|| Mutex::new(HashSet::new()))
    }

    pub(crate) fn enter(id: &str) {
        set()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id.to_owned());
    }

    pub(crate) fn leave(id: &str) {
        set()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(id);
    }

    pub(crate) fn is_registered(id: &str) -> bool {
        set()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(id)
    }
}

/// Thread-local forced spawn failure. It runs the same rollback branch as a
/// real spawn error, which cannot be produced deterministically otherwise.
#[cfg(test)]
pub(crate) mod startup_injection {
    use std::cell::RefCell;

    thread_local! {
        static FAIL_SPAWN: RefCell<bool> = const { RefCell::new(false) };
    }

    pub(crate) fn fail_next_spawn() {
        FAIL_SPAWN.with(|flag| *flag.borrow_mut() = true);
    }

    pub(super) fn take_spawn_failure() -> bool {
        FAIL_SPAWN.with(|flag| std::mem::take(&mut *flag.borrow_mut()))
    }
}

/// Test-only barrier between a successful gate spawn and session publication.
///
/// When the owner helper sets `DCL_PTY_PREPUBLISH_BARRIER`, `start` records the
/// exact owner and gate identities and then blocks forever: only the parent
/// test's `SIGKILL` ends it, so no `Drop` runs in the owner. At that instant the
/// gate is a real PTY session leader that has received neither the
/// specification nor `GO`, so it cannot have run the user program. This is
/// compiled only under `cfg(test)`; a production build has no such hook and
/// model input cannot reach it.
#[cfg(test)]
pub(crate) mod prepublication_barrier {
    use super::*;
    use serde_json::json;

    pub(crate) const BARRIER_ENV: &str = "DCL_PTY_PREPUBLISH_BARRIER";

    pub(super) fn reach(
        directory: &Path,
        task_id: &str,
        session: &str,
        gate_pid: u32,
    ) -> Result<()> {
        let Some(path) = std::env::var_os(BARRIER_ENV).map(PathBuf::from) else {
            return Ok(());
        };
        let owner_pid = std::process::id();
        let owner_start = pty_transport::process_start_tick(owner_pid)?.unwrap_or(0);
        let gate_start = pty_transport::process_start_tick(gate_pid)?.unwrap_or(0);
        write_private(
            &path,
            json!({
                "version": 1,
                "owner_pid": owner_pid,
                "owner_start_tick": owner_start,
                "gate_pid": gate_pid,
                "gate_start_tick": gate_start,
                "task_id": task_id,
                "session": session,
                "task_directory": directory,
            })
            .to_string()
            .as_bytes(),
        )?;
        // Block until the parent test kills this process. The gate stays
        // blocked on its private socket and cannot exec anything.
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

#[cfg(test)]
impl OwnerGuard {
    pub(crate) fn inject_final_tail(&self, bytes: &[u8]) {
        *lock(&self.shared.final_tail) = bytes.to_vec();
    }

    pub(crate) fn inject_output_fault(&self, mode: u8) {
        self.shared.output_fault.store(mode, Ordering::Release);
    }

    pub(crate) fn fail_task_update(&self) {
        self.shared.fail_task_update.store(true, Ordering::Release);
    }
}

/// A test-owned fixture: task, endpoint, owner guard and exact process identity.
#[cfg(test)]
mod tests {
    use super::*;

    const BUDGET: Duration = Duration::from_secs(20);

    /// The four gate tests mutate process-global gates; serialize only those
    /// tests so a concurrent clear cannot release another test's barrier.
    static GATE_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn gate_serial() -> std::sync::MutexGuard<'static, ()> {
        GATE_SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn spec_with_cwd(script: &str, cwd: &Path) -> SpawnSpec {
        SpawnSpec {
            executable: std::ffi::OsString::from("/bin/bash"),
            arguments: vec![
                std::ffi::OsString::from("--noprofile"),
                std::ffi::OsString::from("--norc"),
                std::ffi::OsString::from("-c"),
                std::ffi::OsString::from(script),
            ],
            cwd: cwd.to_path_buf(),
            environment: vec![(
                std::ffi::OsString::from("PATH"),
                std::ffi::OsString::from("/usr/bin:/bin"),
            )],
            rows: 24,
            cols: 80,
        }
    }

    fn envelope(fixture: &Fixture, operation: &str) -> Value {
        envelope_with(fixture, operation, &uuid::Uuid::new_v4().to_string())
    }

    /// An envelope with a known request id, so a test can wait for the owner's
    /// claim notification instead of guessing with a sleep.
    fn envelope_with(fixture: &Fixture, operation: &str, request_id: &str) -> Value {
        let terminal: Value =
            serde_json::from_str(&std::fs::read_to_string(fixture.terminal_path()).unwrap())
                .unwrap();
        json!({
            "version": 1,
            "token": terminal["token"],
            "generation": terminal["generation"],
            "task_id": fixture.id,
            "session": fixture.session,
            "request_id": request_id,
            "operation": operation
        })
    }

    fn send_frame(stream: &mut UnixStream, value: &Value) {
        let body = value.to_string();
        stream
            .write_all(&(body.len() as u32).to_be_bytes())
            .unwrap();
        stream.write_all(body.as_bytes()).unwrap();
    }

    fn read_reply(stream: &mut UnixStream) -> Value {
        let mut length = [0u8; 4];
        stream.read_exact(&mut length).unwrap();
        let length = u32::from_be_bytes(length) as usize;
        let mut body = vec![0u8; length];
        stream.read_exact(&mut body).unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    /// Bounded reply read: `None` means the connection produced nothing before
    /// the wait expired, which is a real observation (the accept thread never
    /// admitted it), not a panic that hides which connection was stranded.
    fn read_reply_bounded(stream: &mut UnixStream, wait: Duration) -> Option<Value> {
        stream.set_read_timeout(Some(wait)).ok()?;
        let mut length = [0u8; 4];
        stream.read_exact(&mut length).ok()?;
        let length = u32::from_be_bytes(length) as usize;
        let mut body = vec![0u8; length];
        stream.read_exact(&mut body).ok()?;
        serde_json::from_slice(&body).ok()
    }

    fn session_has_live_members(session: u32) -> bool {
        let Ok(entries) = std::fs::read_dir("/proc") else {
            return true;
        };
        for entry in entries.flatten() {
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            else {
                continue;
            };
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
                continue;
            };
            let Some((_, rest)) = stat.rsplit_once(')') else {
                continue;
            };
            let fields: Vec<&str> = rest.split_whitespace().collect();
            if fields.len() < 20 {
                continue;
            }
            let state = fields[0];
            let sid: u32 = fields[3].parse().unwrap_or(0);
            if sid == session && state != "Z" {
                return true;
            }
        }
        false
    }

    fn wait_for<T>(what: &str, deadline: Instant, mut check: impl FnMut() -> Option<T>) -> T {
        loop {
            if let Some(value) = check() {
                return value;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    struct Fixture {
        _directory: tempfile::TempDir,
        root: PathBuf,
        session: String,
        id: String,
        child_pid: u32,
        guard: Option<OwnerGuard>,
    }

    impl Fixture {
        fn start(script: &str) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().to_path_buf();
            let spec = spec_with_cwd(script, &root);
            Self::from_spec(directory, spec, script)
        }

        fn from_spec(directory: tempfile::TempDir, spec: SpawnSpec, command_text: &str) -> Self {
            let root = directory.path().to_path_buf();
            let session = uuid::Uuid::new_v4().to_string();
            let (value, guard) =
                super::start(spec, &root, &session, "pty fixture", command_text).unwrap();
            let id = value["task_id"].as_str().unwrap().to_owned();
            let child_pid = value["terminal_pid"].as_u64().unwrap() as u32;
            Self {
                _directory: directory,
                root,
                session,
                id,
                child_pid,
                guard: Some(guard),
            }
        }

        fn directory(&self) -> PathBuf {
            self.root
                .join("shell-tasks")
                .join(&self.session)
                .join(&self.id)
        }

        fn terminal_path(&self) -> PathBuf {
            self.directory().join("terminal.json")
        }

        fn terminal_state(&self) -> Option<Value> {
            std::fs::read_to_string(self.directory().join("terminal-state.json"))
                .ok()
                .and_then(|text| serde_json::from_str(&text).ok())
        }

        fn endpoint(&self) -> PathBuf {
            let terminal: Value =
                serde_json::from_str(&std::fs::read_to_string(self.terminal_path()).unwrap())
                    .unwrap();
            PathBuf::from(terminal["endpoint"].as_str().unwrap())
        }

        fn stdout(&self) -> String {
            std::fs::read_to_string(self.directory().join("stdout.log")).unwrap_or_default()
        }

        fn call(&self, input: Value) -> Value {
            self.call_with(input, None)
        }

        fn call_with(&self, mut input: Value, cancelled: Option<&AtomicBool>) -> Value {
            input["task_id"] = json!(self.id);
            super::execute(&input, &self.root, &self.session, cancelled).unwrap()
        }

        fn wait_output(&self, needle: &str) -> String {
            wait_for(needle, Instant::now() + BUDGET, || {
                let log = self.stdout();
                log.contains(needle).then_some(log)
            })
        }

        fn task_get(&self) -> Value {
            super::super::tasks::execute_for_session(
                "TaskGet",
                json!({"task_id": self.id, "block": false}),
                &self.root,
                &self.session,
            )
            .unwrap()
        }

        /// Reconstruct the stdout log through the bounded page API.
        fn paged_stdout(&self) -> String {
            let mut cursor: Option<String> = None;
            let mut collected = String::new();
            loop {
                let mut input = json!({"task_id":self.id,"max_output_bytes":4096});
                if let Some(value) = &cursor {
                    input["cursor"] = json!(value);
                }
                let page = super::super::background_shell::control(
                    "TaskGet",
                    &input,
                    &self.root,
                    &self.session,
                )
                .unwrap()
                .unwrap();
                assert_eq!(page["output_gap"], Value::Null, "{page}");
                collected.push_str(page["stdout"].as_str().unwrap_or_default());
                if page["has_more"] != true {
                    break;
                }
                cursor = Some(page["next_cursor"].as_str().unwrap().to_owned());
            }
            collected
        }

        fn stop_and_wait(&mut self) -> Value {
            let guard = self.guard.as_mut().expect("this fixture owns its guard");
            guard
                .stop_and_join(Instant::now() + BUDGET)
                .expect("the owner stops and joins")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Some(guard) = self.guard.as_mut() {
                // The guard performs the bounded, owned normal shutdown.
                let _ = guard.stop_and_join(Instant::now() + Duration::from_secs(15));
                return;
            }
            // The owner lives in another process (cross-process test): use the
            // documented cancel marker and wait for its terminal state. Nothing
            // is signalled by name.
            let _ = std::fs::write(self.directory().join("cancel"), b"cancel");
            let deadline = Instant::now() + Duration::from_secs(15);
            while self.terminal_state().is_none() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            if self.terminal_state().is_none() || session_has_live_members(self.child_pid) {
                eprintln!(
                    "PTY fixture cleanup incomplete: task={} child_pid={} session_members={}",
                    self.id,
                    self.child_pid,
                    session_has_live_members(self.child_pid)
                );
            }
        }
    }

    /// A terminal whose child does not read until the gate file exists, so a
    /// 64 KiB write fills the input queue and blocks.
    fn blocked_writer_script(gate: &Path, consumed: &Path) -> String {
        format!(
            "stty raw -echo\n\
             printf 'READY\\n'\n\
             while [ ! -e '{}' ]; do sleep 0.01; done\n\
             cat > '{}'\n\
             sleep 30\n",
            gate.display(),
            consumed.display()
        )
    }

    /// Open an authorized write of [`MAX_DATA_BYTES`] with a long budget and
    /// wait for the explicit claim and partial-write notifications.
    fn start_blocked_write(fixture: &Fixture, timeout_ms: u64) -> (String, UnixStream) {
        let request_id = uuid::Uuid::new_v4().to_string();
        let mut stream = UnixStream::connect(fixture.endpoint()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let mut request = envelope_with(fixture, "write", &request_id);
        request["data"] = json!("a".repeat(MAX_DATA_BYTES));
        request["timeout_ms"] = json!(timeout_ms);
        send_frame(&mut stream, &request);
        assert!(
            claim_probe::wait_claimed(&request_id, Instant::now() + BUDGET),
            "the owner never claimed the blocked write"
        );
        assert!(
            claim_probe::wait_partial(&request_id, Instant::now() + BUDGET),
            "the blocked write accepted no bytes"
        );
        let accepted = claim_probe::accepted_bytes(&request_id);
        assert!(
            accepted > 0 && accepted < MAX_DATA_BYTES,
            "the non-reading terminal did not produce a partial write: {accepted} bytes"
        );
        (request_id, stream)
    }

    /// Queue a second write behind the blocked one; the queued notification is
    /// the proof that it was admitted, not a sleep.
    fn queue_write(fixture: &Fixture, data: &str, timeout_ms: u64) -> (String, UnixStream) {
        let request_id = uuid::Uuid::new_v4().to_string();
        let mut stream = UnixStream::connect(fixture.endpoint()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let mut request = envelope_with(fixture, "write", &request_id);
        request["data"] = json!(data);
        request["timeout_ms"] = json!(timeout_ms);
        send_frame(&mut stream, &request);
        assert!(
            claim_probe::wait_queued(&request_id, Instant::now() + BUDGET),
            "the second write was never queued"
        );
        (request_id, stream)
    }

    /// Clear every test gate even when an assertion panics.
    struct ClearGates;
    impl Drop for ClearGates {
        fn drop(&mut self) {
            serve_gate::clear();
            scan_gate::clear();
            admit_gate::clear();
            pre_stop_gate::clear();
        }
    }

    /// Clear the thread-local startup probe.
    struct ClearStartupProbe;
    impl Drop for ClearStartupProbe {
        fn drop(&mut self) {
            startup_probe::clear();
        }
    }

    #[test]
    fn interactive_write_and_resize_reach_the_same_process() {
        let fixture = Fixture::start(
            "stty size\n\
             printf 'READY\\n'\n\
             read -r line\n\
             printf 'GOT:%s\\n' \"$line\"\n\
             read -r again\n\
             stty size\n\
             printf 'DONE\\n'\n\
             sleep 30\n",
        );
        let first = fixture.wait_output("READY");
        assert!(first.contains("24 80"), "{first}");

        let reply = fixture.call(json!({"data":"hello\n"}));
        assert_eq!(reply["bytes_written"], 6, "{reply}");
        assert_eq!(reply["write_complete"], true, "{reply}");
        fixture.wait_output("GOT:hello");

        let resized = fixture.call(json!({"resize":{"rows":40,"cols":120}}));
        assert_eq!(resized["rows"], 40, "{resized}");
        assert_eq!(resized["cols"], 120, "{resized}");
        fixture.call(json!({"data":"\n"}));
        let done = fixture.wait_output("DONE");
        assert!(
            done.contains("40 120"),
            "the child did not see the resize: {done}"
        );

        // Same terminal, same process: the recorded PID is alive and is the
        // session leader.
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", fixture.child_pid)).unwrap();
        let fields: Vec<&str> = stat
            .rsplit_once(')')
            .unwrap()
            .1
            .split_whitespace()
            .collect();
        assert_ne!(fields[0], "Z");
        assert_eq!(fields[3].parse::<u32>().unwrap(), fixture.child_pid);
    }

    /// Helper role for the cross-process test: this same test binary, started
    /// with `DCL_PTY_OWNER_CHILD`.
    fn helper_child(config: &str) {
        let config: Value = serde_json::from_str(config).unwrap();
        let root = PathBuf::from(config["root"].as_str().unwrap());
        let session = config["session"].as_str().unwrap().to_owned();
        let script = config["script"].as_str().unwrap();
        let spec = spec_with_cwd(script, &root);
        let (value, mut guard) = super::start(spec, &root, &session, "helper pty", script).unwrap();
        let id = value["task_id"].as_str().unwrap().to_owned();
        let pid = value["terminal_pid"].as_u64().unwrap();
        std::fs::write(
            config["info"].as_str().unwrap(),
            json!({"task_id":id,"session":session,"terminal_pid":pid}).to_string(),
        )
        .unwrap();
        let finish = PathBuf::from(config["finish"].as_str().unwrap());
        let deadline = Instant::now() + Duration::from_secs(30);
        while !finish.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = guard.stop_and_join(Instant::now() + Duration::from_secs(15));
    }

    #[test]
    fn a_separate_process_reaches_the_same_pty() {
        if let Ok(config) = std::env::var("DCL_PTY_OWNER_CHILD") {
            helper_child(&config);
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let session = uuid::Uuid::new_v4().to_string();
        let info = root.join("helper.json");
        let finish = root.join("finish");
        let script = "printf 'CHILD=%s\\n' \"$$\"\n\
                      while read -r line; do printf 'ECHO:%s\\n' \"$line\"; done\n";
        let config = json!({
            "root": root,
            "session": session,
            "script": script,
            "info": info,
            "finish": finish
        });
        let helper = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tools::pty_owner::tests::a_separate_process_reaches_the_same_pty",
                "--nocapture",
            ])
            .env("DCL_PTY_OWNER_CHILD", config.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        // An RAII guard owns exactly this helper PID.
        struct Helper(Option<std::process::Child>);
        impl Drop for Helper {
            fn drop(&mut self) {
                if let Some(child) = self.0.as_mut() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
            }
        }
        let mut helper = Helper(Some(helper));
        let started = wait_for("the helper task", Instant::now() + BUDGET, || {
            std::fs::read_to_string(&info)
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        });
        let task_id = started["task_id"].as_str().unwrap().to_owned();
        let child_pid = started["terminal_pid"].as_u64().unwrap() as u32;
        let fixture = Fixture {
            _directory: directory,
            root: root.clone(),
            session: session.clone(),
            id: task_id.clone(),
            child_pid,
            guard: None,
        };
        fixture.wait_output(&format!("CHILD={child_pid}"));

        // This process is not the owner: the owner thread lives in the helper.
        let reply = fixture.call(json!({"data":"cross-process\n"}));
        assert_eq!(reply["ok"], true, "{reply}");
        assert_eq!(reply["bytes_written"], 14, "{reply}");
        let log = fixture.wait_output("ECHO:cross-process");
        assert!(log.contains(&format!("CHILD={child_pid}")), "{log}");

        std::fs::write(&finish, b"done").unwrap();
        let mut child = helper.0.take().unwrap();
        let status = child.wait().unwrap();
        assert!(status.success(), "the helper exited with {status}");
        let state = wait_for("the terminal state", Instant::now() + BUDGET, || {
            fixture.terminal_state()
        });
        assert_eq!(state["cleanup_complete"], true, "{state}");
        assert_eq!(state["cancelled"], true, "{state}");
    }

    #[test]
    fn a_foreign_session_and_a_stale_owner_are_refused() {
        let fixture = Fixture::start("printf 'READY\\n'; sleep 30");
        fixture.wait_output("READY");

        // A different session has no such record at all.
        let other = uuid::Uuid::new_v4().to_string();
        let error = super::execute(
            &json!({"task_id": fixture.id, "data": "x\n"}),
            &fixture.root,
            &other,
            None,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("not found") || message.contains("Task"),
            "{message}"
        );

        // A record without a held lease is refused before any connection, even
        // though its directory and heartbeat exist.
        let stale_session = uuid::Uuid::new_v4().to_string();
        let task = super::super::tasks::execute_for_shell_owner(
            "TaskCreate",
            json!({"subject":"stale","metadata":{"managed_shell":true,"interaction":INTERACTION}}),
            &fixture.root,
            &stale_session,
        )
        .unwrap();
        let stale_id = task["id"].as_str().unwrap();
        super::super::tasks::execute_for_shell_owner(
            "TaskUpdate",
            json!({"id":stale_id,"status":"running"}),
            &fixture.root,
            &stale_session,
        )
        .unwrap();
        let stale_directory = fixture
            .root
            .join("shell-tasks")
            .join(&stale_session)
            .join(stale_id);
        std::fs::create_dir_all(&stale_directory).unwrap();
        std::fs::write(stale_directory.join("heartbeat"), b"running").unwrap();
        let error = super::execute(
            &json!({"task_id":stale_id, "data": "x\n"}),
            &fixture.root,
            &stale_session,
            None,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("stdin is unavailable") || message.contains("lease"),
            "{message}"
        );
    }

    #[test]
    fn a_replaced_endpoint_is_refused_by_identity() {
        let fixture = Fixture::start("printf 'READY\\n'; sleep 30");
        fixture.wait_output("READY");
        let endpoint = fixture.endpoint();
        // Same user, same path, same permissions, different socket: only the
        // recorded dev/ino distinguishes them.
        std::fs::remove_file(&endpoint).unwrap();
        let replacement = UnixListener::bind(&endpoint).unwrap();
        std::fs::set_permissions(&endpoint, std::fs::Permissions::from_mode(0o600)).unwrap();
        let error = super::execute(
            &json!({"task_id": fixture.id, "data": "x\n"}),
            &fixture.root,
            &fixture.session,
            None,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("replaced"), "{message}");
        drop(replacement);
    }

    /// The check and the connect are separate steps. A swap in between must be
    /// caught by the post-connect identity check, not slip through.
    #[test]
    fn an_endpoint_swapped_between_check_and_connect_is_refused() {
        let fixture = Fixture::start("printf 'READY\\n'; sleep 30");
        fixture.wait_output("READY");
        let endpoint = fixture.endpoint();
        let swapped = std::sync::Arc::new(std::sync::Mutex::new(None));
        let slot = swapped.clone();
        let path = endpoint.clone();
        swap_probe::install(Box::new(move || {
            std::fs::remove_file(&path).unwrap();
            let listener = UnixListener::bind(&path).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            *slot.lock().unwrap() = Some(listener);
        }));
        struct ClearProbe;
        impl Drop for ClearProbe {
            fn drop(&mut self) {
                swap_probe::clear();
            }
        }
        let _clear = ClearProbe;
        let error = super::execute(
            &json!({"task_id": fixture.id, "data": "x\n"}),
            &fixture.root,
            &fixture.session,
            None,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("replaced while connecting"),
            "the swap was not detected: {message}"
        );
        assert!(
            swapped.lock().unwrap().is_some(),
            "the probe did not install a replacement endpoint"
        );
    }

    /// A full backlog must not spawn a connector thread or hold a descriptor.
    ///
    /// The measurement is exact only in a process with no other tests running,
    /// so the body runs in a child of this same binary; the parent asserts the
    /// child's exit status instead of reading a shared process's counters.
    #[test]
    fn a_full_backlog_connect_times_out_without_leaking_threads_or_descriptors() {
        if std::env::var("DCL_PTY_BACKLOG_CHILD").is_ok() {
            backlog_probe();
            return;
        }
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tools::pty_owner::tests::a_full_backlog_connect_times_out_without_leaking_threads_or_descriptors",
                "--nocapture",
            ])
            .env("DCL_PTY_BACKLOG_CHILD", "1")
            .status()
            .unwrap();
        assert!(status.success(), "the isolated backlog probe failed");
    }

    fn backlog_probe() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("backlog.sock");
        // A listener with a zero-length backlog: one queued connection fills it
        // and the next nonblocking connect gets EAGAIN, which is exactly the
        // saturated-backlog case that used to spawn a connector thread.
        let address = rustix::net::SocketAddrUnix::new(&path).unwrap();
        let listener = rustix::net::socket_with(
            rustix::net::AddressFamily::UNIX,
            rustix::net::SocketType::STREAM,
            rustix::net::SocketFlags::CLOEXEC | rustix::net::SocketFlags::NONBLOCK,
            None,
        )
        .unwrap();
        rustix::net::bind(&listener, &address).unwrap();
        rustix::net::listen(&listener, 0).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let threads = count_threads();
        let baseline = count_descriptors();
        let mut held = Vec::new();
        let mut timeouts = 0;
        for _ in 0..50 {
            let before = count_descriptors();
            match connect_bounded(&path, Instant::now() + Duration::from_millis(20), None) {
                // Keep the socket so its backlog slot stays consumed.
                Ok(stream) => held.push(stream),
                Err(error) => {
                    assert!(
                        format!("{error:#}").contains("deadline"),
                        "expected a bounded timeout, got {error:#}"
                    );
                    assert_eq!(
                        count_descriptors(),
                        before,
                        "a timed-out connect leaked a descriptor"
                    );
                    timeouts += 1;
                }
            }
        }
        assert!(timeouts > 0, "a saturated backlog never timed out");
        assert_eq!(
            count_threads(),
            threads,
            "timed-out connects leaked connector threads"
        );
        // The only growth is the sockets we deliberately kept open.
        assert_eq!(
            count_descriptors(),
            baseline + held.len(),
            "timed-out connects leaked descriptors"
        );
        drop(held);
        drop(listener);
    }

    fn count_descriptors() -> usize {
        std::fs::read_dir("/proc/self/fd")
            .map(|entries| entries.count())
            .unwrap_or(0)
    }

    fn count_threads() -> usize {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|status| {
                status
                    .lines()
                    .find_map(|line| line.strip_prefix("Threads:"))
                    .and_then(|value| value.trim().parse::<usize>().ok())
            })
            .unwrap_or(0)
    }

    #[test]
    fn oversized_unauthenticated_and_mismatched_frames_are_refused() {
        let fixture = Fixture::start("printf 'READY\\n'; sleep 30");
        fixture.wait_output("READY");
        let endpoint = fixture.endpoint();

        // Oversized length prefix: refused before any body is read.
        let mut stream = UnixStream::connect(&endpoint).unwrap();
        stream.write_all(&u32::MAX.to_be_bytes()).unwrap();
        let reply = read_reply(&mut stream);
        assert_eq!(reply["ok"], false, "{reply}");
        assert!(
            reply["error"].as_str().unwrap().contains("frame"),
            "{reply}"
        );
        drop(stream);

        // Wrong token.
        let mut stream = UnixStream::connect(&endpoint).unwrap();
        let mut request = envelope(&fixture, "write");
        request["token"] = json!("00".repeat(32));
        request["data"] = json!("nope\n");
        send_frame(&mut stream, &request);
        let reply = read_reply(&mut stream);
        assert_eq!(reply["ok"], false, "{reply}");
        assert!(
            reply["error"].as_str().unwrap().contains("token"),
            "{reply}"
        );

        // Right token, another task.
        let mut stream = UnixStream::connect(&endpoint).unwrap();
        let mut request = envelope(&fixture, "write");
        request["task_id"] = json!(uuid::Uuid::new_v4().to_string());
        request["data"] = json!("nope\n");
        send_frame(&mut stream, &request);
        let reply = read_reply(&mut stream);
        assert_eq!(reply["ok"], false, "{reply}");
        assert!(reply["error"].as_str().unwrap().contains("task"), "{reply}");

        // Right token, another generation.
        let mut stream = UnixStream::connect(&endpoint).unwrap();
        let mut request = envelope(&fixture, "write");
        request["generation"] = json!(uuid::Uuid::new_v4().to_string());
        request["data"] = json!("nope\n");
        send_frame(&mut stream, &request);
        let reply = read_reply(&mut stream);
        assert_eq!(reply["ok"], false, "{reply}");
        assert!(
            reply["error"].as_str().unwrap().contains("generation"),
            "{reply}"
        );

        // The owner survived all of it and never wrote the rejected data.
        let reply = fixture.call(json!({"data":"still-here\n"}));
        assert_eq!(reply["ok"], true, "{reply}");
        let log = fixture.wait_output("still-here");
        assert!(!log.contains("nope"), "{log}");
    }

    #[test]
    fn a_cancelled_call_writes_nothing() {
        let fixture = Fixture::start("stty -echo; printf 'READY\\n'; cat; sleep 30");
        fixture.wait_output("READY");
        let cancelled = AtomicBool::new(true);
        let reply = fixture.call_with(json!({"data":"CANCELLED-MARKER\n"}), Some(&cancelled));
        assert_eq!(reply["cancelled"], true, "{reply}");
        assert_eq!(reply["bytes_written"], 0, "{reply}");
        assert_eq!(reply["queued"], false, "{reply}");
        std::thread::sleep(Duration::from_millis(100));
        assert!(!fixture.stdout().contains("CANCELLED-MARKER"));
    }

    /// A request may be admitted while another one still runs. Its caller
    /// deadline then passes before the owner reaches it: the `QUEUED ->
    /// CANCELLED` transition is what allows `not_executed`, and nothing is
    /// written.
    #[test]
    fn an_admitted_request_cancelled_before_execution_writes_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let gate = root.join("gate");
        let consumed = root.join("consumed.bin");
        let script = format!(
            "stty raw -echo\n\
             printf 'READY\\n'\n\
             while [ ! -e '{}' ]; do sleep 0.01; done\n\
             cat > '{}'\n\
             sleep 30\n",
            gate.display(),
            consumed.display()
        );
        let fixture = Fixture::from_spec(directory, spec_with_cwd(&script, &root), &script);
        fixture.wait_output("READY");

        // A long blocking write fills the raw terminal, so the owner is busy
        // when the second request is admitted. The claim notification proves
        // the first write is in flight; a sleep would not.
        let first_id = uuid::Uuid::new_v4().to_string();
        let mut long = UnixStream::connect(fixture.endpoint()).unwrap();
        long.set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        let mut request = envelope_with(&fixture, "write", &first_id);
        request["data"] = json!("a".repeat(MAX_DATA_BYTES));
        request["timeout_ms"] = json!(3000);
        send_frame(&mut long, &request);
        assert!(
            claim_probe::wait_claimed(&first_id, Instant::now() + BUDGET),
            "the owner never claimed the first request"
        );

        let mut short = UnixStream::connect(fixture.endpoint()).unwrap();
        short
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        let mut request = envelope(&fixture, "write");
        request["data"] = json!("SECOND-MARKER\n");
        request["timeout_ms"] = json!(150);
        send_frame(&mut short, &request);
        let reply = read_reply(&mut short);
        assert_eq!(reply["ok"], false, "{reply}");
        assert_eq!(reply["outcome"], "not_executed", "{reply}");
        assert_eq!(reply["may_have_been_written"], false, "{reply}");
        assert!(
            claim_probe::wait_finished(&first_id, Instant::now() + BUDGET),
            "the first request never finished"
        );

        let long_reply = read_reply(&mut long);
        assert_eq!(long_reply["ok"], true, "{long_reply}");
        let accepted = long_reply["bytes_written"].as_u64().unwrap() as usize;
        assert!(accepted > 0 && accepted < MAX_DATA_BYTES, "{long_reply}");

        // The child consumed exactly what the first write accepted; the
        // cancelled request never reached the terminal.
        std::fs::write(&gate, b"go").unwrap();
        wait_for("the consumed prefix", Instant::now() + BUDGET, || {
            std::fs::metadata(&consumed)
                .ok()
                .map(|metadata| metadata.len())
                .filter(|len| *len >= accepted as u64)
        });
        std::thread::sleep(Duration::from_millis(200));
        let bytes = std::fs::read(&consumed).unwrap();
        assert_eq!(bytes.len(), accepted, "a late request added bytes");
        assert!(bytes.iter().all(|byte| *byte == b'a'), "unexpected bytes");
    }

    /// Execution already started when the caller disappeared: the only honest
    /// statement is "unknown", and the accepted prefix really is in the child.
    #[test]
    fn an_executing_request_is_never_reported_as_not_executed() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let gate = root.join("gate");
        let consumed = root.join("consumed.bin");
        let script = format!(
            "stty raw -echo\n\
             printf 'READY\\n'\n\
             while [ ! -e '{}' ]; do sleep 0.01; done\n\
             cat > '{}'\n\
             sleep 30\n",
            gate.display(),
            consumed.display()
        );
        let fixture = Fixture::from_spec(directory, spec_with_cwd(&script, &root), &script);
        fixture.wait_output("READY");

        // The owner claims and starts the write, then the caller goes away
        // without reading the reply. The claim notification is the proof that
        // execution began; a sleep would not be.
        let request_id = uuid::Uuid::new_v4().to_string();
        let mut stream = UnixStream::connect(fixture.endpoint()).unwrap();
        let mut request = envelope_with(&fixture, "write", &request_id);
        request["data"] = json!("b".repeat(MAX_DATA_BYTES));
        request["timeout_ms"] = json!(800);
        send_frame(&mut stream, &request);
        assert!(
            claim_probe::wait_claimed(&request_id, Instant::now() + BUDGET),
            "the owner never claimed the request"
        );
        drop(stream);
        assert!(
            claim_probe::wait_finished(&request_id, Instant::now() + BUDGET),
            "the claimed request never finished"
        );

        // The write reached its deadline while the child was not reading, so
        // the accepted prefix is partial. Releasing the child now must yield
        // exactly that prefix.
        std::fs::write(&gate, b"go").unwrap();
        let bytes = wait_for("the accepted prefix", Instant::now() + BUDGET, || {
            let bytes = std::fs::read(&consumed).ok()?;
            (!bytes.is_empty()).then_some(bytes)
        });
        std::thread::sleep(Duration::from_millis(200));
        let bytes = std::fs::read(&consumed).unwrap();
        assert!(
            !bytes.is_empty() && bytes.len() < MAX_DATA_BYTES,
            "the request either did not execute or was not partial: {} bytes",
            bytes.len()
        );
        assert!(bytes.iter().all(|byte| *byte == b'b'), "unexpected bytes");

        // The owner survived the lost reply.
        let reply = fixture.call(json!({"data":"after-loss\n"}));
        assert_eq!(reply["ok"], true, "{reply}");
    }

    #[test]
    fn a_partial_write_reports_progress_and_a_lost_reply_is_not_replayed() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let gate = root.join("gate");
        let consumed = root.join("consumed.bin");
        let script = format!(
            "stty -echo\n\
             printf 'READY\\n'\n\
             while [ ! -e '{}' ]; do sleep 0.01; done\n\
             cat > '{}'\n\
             sleep 30\n",
            gate.display(),
            consumed.display()
        );
        let fixture = Fixture::from_spec(directory, spec_with_cwd(&script, &root), &script);
        fixture.wait_output("READY");

        // Send a complete request, wait for the owner to claim it, then close
        // without reading the reply: the caller learns nothing, so it must not
        // assume the write did not happen. The claim notification is the proof
        // that execution began; a caller that left before the claim could
        // legitimately be cancelled instead, which this test does not exercise.
        let request_id = uuid::Uuid::new_v4().to_string();
        let mut stream = UnixStream::connect(fixture.endpoint()).unwrap();
        let mut request = envelope_with(&fixture, "write", &request_id);
        request["data"] = json!("LOST-REPLY-MARKER\n");
        send_frame(&mut stream, &request);
        assert!(
            claim_probe::wait_claimed(&request_id, Instant::now() + BUDGET),
            "the owner never claimed the request"
        );
        drop(stream);
        assert!(
            claim_probe::wait_finished(&request_id, Instant::now() + BUDGET),
            "the claimed request never finished"
        );

        std::fs::write(&gate, b"go").unwrap();
        let text = wait_for("the marker", Instant::now() + BUDGET, || {
            let text = std::fs::read_to_string(&consumed).ok()?;
            text.contains("LOST-REPLY-MARKER").then_some(text)
        });
        assert_eq!(
            text.matches("LOST-REPLY-MARKER").count(),
            1,
            "the request was replayed: {text:?}"
        );

        // The owner kept running: a later call still works.
        let reply = fixture.call(json!({"data":"AFTER-LOSS\n"}));
        assert_eq!(reply["ok"], true, "{reply}");
    }

    #[test]
    fn a_request_transmits_the_remaining_budget() {
        let deadline = Instant::now() + Duration::from_millis(500);
        std::thread::sleep(Duration::from_millis(20));
        let budget = remaining_budget(deadline);
        assert!(budget < Duration::from_millis(500));
        assert!(budget > Duration::ZERO);
        let body = Request::Write {
            data: "x".into(),
            eof: false,
        }
        .encode("req", "token", "gen", "task", "session", budget);
        let value: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(value["timeout_ms"], json!(budget.as_millis() as u64));
        assert_eq!(value["generation"], "gen");
        assert_eq!(value["task_id"], "task");
        assert_eq!(value["session"], "session");
    }

    #[test]
    fn task_get_pages_merged_output_without_duplicating_the_tail() {
        let fixture = Fixture::start(
            "printf 'OUT-A\\n'\n\
             printf 'ERR-B\\n' >&2\n\
             printf 'héllo 🌍\\n'\n\
             sleep 30\n",
        );
        fixture.wait_output("héllo");
        let stderr = std::fs::read_to_string(fixture.directory().join("stderr.log")).unwrap();
        assert!(
            stderr.is_empty(),
            "a separate stderr stream appeared: {stderr:?}"
        );
        let raw = fixture.stdout();
        assert!(raw.contains("OUT-A") && raw.contains("ERR-B"), "{raw}");

        // Page through the task with the existing cursor API.
        let mut cursor: Option<String> = None;
        let mut collected = String::new();
        loop {
            let mut input = json!({"task_id":fixture.id,"max_output_bytes":4});
            if let Some(value) = &cursor {
                input["cursor"] = json!(value);
            }
            let page = super::super::background_shell::control(
                "TaskGet",
                &input,
                &fixture.root,
                &fixture.session,
            )
            .unwrap()
            .unwrap();
            assert_eq!(page["output_gap"], Value::Null, "{page}");
            collected.push_str(page["stdout"].as_str().unwrap_or_default());
            if page["has_more"] != true {
                break;
            }
            cursor = Some(page["next_cursor"].as_str().unwrap().to_owned());
        }
        assert_eq!(
            collected, raw,
            "paging lost, reordered or duplicated output"
        );
        let snapshot = fixture.task_get();
        assert_eq!(snapshot["metadata"]["merged_output"], true, "{snapshot}");
    }

    #[tokio::test]
    async fn a_task_get_timeout_leaves_the_owner_and_child_alive() {
        let fixture = Fixture::start(
            "printf 'READY\\n'; while read -r line; do printf 'GOT:%s\\n' \"$line\"; done",
        );
        fixture.wait_output("READY");
        let value = super::super::background_shell::control_async(
            "TaskGet",
            &json!({"task_id":fixture.id,"block":true,"timeout":120}),
            &fixture.root,
            &fixture.session,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(value["retrieval_status"], "timeout", "{value}");
        assert!(session_has_live_members(fixture.child_pid));
        let reply = fixture.call(json!({"data":"after-timeout\n"}));
        assert_eq!(reply["ok"], true, "{reply}");
        fixture.wait_output("GOT:after-timeout");
    }

    #[test]
    fn task_stop_cleans_job_groups_and_marks_terminal_once() {
        let fixture = Fixture::start(
            "set -m\n\
             printf 'READY\\n'\n\
             sleep 30 & printf 'BG=%s\\n' \"$!\"\n\
             sleep 30 | cat & printf 'PIPE=%s\\n' \"$!\"\n\
             sleep 30\n",
        );
        let log = fixture.wait_output("PIPE=");
        assert!(log.contains("READY"), "{log}");
        let stopped = super::super::tasks::execute_for_session(
            "TaskStop",
            json!({"task_id":fixture.id}),
            &fixture.root,
            &fixture.session,
        )
        .unwrap();
        // The managed-shell TaskStop path reports the cancellation request; the
        // record becomes terminal only after the owner's cleanup.
        assert_eq!(stopped["cancellation_requested"], true, "{stopped}");
        assert_eq!(stopped["status"], "running", "{stopped}");
        let state = wait_for("the terminal state", Instant::now() + BUDGET, || {
            fixture.terminal_state()
        });
        assert_eq!(state["status"], "cancelled", "{state}");
        assert_eq!(state["cleanup_complete"], true, "{state}");
        assert!(
            !session_has_live_members(fixture.child_pid),
            "the owned session still has live members"
        );
        let task = fixture.task_get();
        assert_eq!(task["status"], "cancelled", "{task}");

        // A repeated stop does not rewrite a terminal record: the status is
        // already terminal, so no new cancellation is requested.
        let again = super::super::tasks::execute_for_session(
            "TaskStop",
            json!({"task_id":fixture.id}),
            &fixture.root,
            &fixture.session,
        )
        .unwrap();
        assert_eq!(again["status"], "cancelled", "{again}");
        assert_eq!(again["cancellation_requested"], false, "{again}");
    }

    #[test]
    fn normal_shutdown_releases_the_endpoint_and_joins_the_owner() {
        let mut fixture = Fixture::start("printf 'READY\\n'; sleep 30");
        fixture.wait_output("READY");
        let endpoint = fixture.endpoint();
        assert!(endpoint.exists());
        let state = fixture.stop_and_wait();
        assert_eq!(state["cleanup_complete"], true, "{state}");
        assert!(
            !endpoint.exists(),
            "the endpoint survived shutdown: {}",
            endpoint.display()
        );
        // The descriptor is history; the state file is the result.
        assert!(fixture.terminal_path().exists());
        assert!(fixture.directory().join("terminal-state.json").exists());
        let probe = super::super::background_shell::OwnerProbe::open(&fixture.directory()).unwrap();
        assert!(!probe.state().is_held());
        // Logs stay readable after the owner is gone.
        let log = fixture.wait_output("READY");
        assert!(log.contains("READY"));
        assert!(fixture.task_get()["stdout"]
            .as_str()
            .unwrap()
            .contains("READY"));
        // The supervisor's record describes what the owner actually recorded.
        // A normal stop is a cancelled task, never a supervisor failure.
        let supervisor: Value = wait_for("the supervisor state", Instant::now() + BUDGET, || {
            std::fs::read_to_string(fixture.directory().join("supervisor-state.json"))
                .ok()
                .and_then(|text| serde_json::from_str(&text).ok())
        });
        assert_eq!(supervisor["owner_lost"], false, "{supervisor}");
        assert_eq!(supervisor["owner_finalized"], true, "{supervisor}");
        assert_eq!(supervisor["cleanup_performed_by"], "owner", "{supervisor}");
        assert_eq!(supervisor["status"], "cancelled", "{supervisor}");
        assert_eq!(supervisor["os_cleanup_complete"], true, "{supervisor}");
        assert_eq!(supervisor["output_finalized"], true, "{supervisor}");
        assert_eq!(fixture.task_get()["status"], "cancelled");
    }

    #[test]
    fn a_normal_owner_finish_is_not_recorded_as_a_supervisor_failure() {
        // The child exits on its own, so the owner finalizes with `completed`
        // before any stop request. The supervisor must report that status, not
        // a generic failure, and TaskGet must keep the owner's terminal state.
        let mut fixture = Fixture::start("printf 'READY\\n'\n");
        fixture.wait_output("READY");
        let terminal: Value = wait_for(
            "the completed terminal state",
            Instant::now() + BUDGET,
            || {
                fixture
                    .terminal_state()
                    .filter(|state| state["status"] == "completed")
            },
        );
        assert_eq!(terminal["exit_code"], 0, "{terminal}");
        let state = fixture.stop_and_wait();
        assert_eq!(state["status"], "completed", "{state}");
        let supervisor: Value = wait_for("the supervisor state", Instant::now() + BUDGET, || {
            std::fs::read_to_string(fixture.directory().join("supervisor-state.json"))
                .ok()
                .and_then(|text| serde_json::from_str(&text).ok())
        });
        assert_eq!(supervisor["owner_lost"], false, "{supervisor}");
        assert_eq!(supervisor["owner_finalized"], true, "{supervisor}");
        assert_eq!(supervisor["status"], "completed", "{supervisor}");
        assert_eq!(supervisor["os_cleanup_complete"], true, "{supervisor}");
        assert_eq!(supervisor["output_finalized"], true, "{supervisor}");
        assert_eq!(fixture.task_get()["status"], "completed");
    }

    /// A supervisor state that is missing or damaged must not turn into a
    /// terminal status, and a half-written record must not be read as success.
    /// A non-PTY managed shell must ignore a same-named file entirely.
    #[test]
    fn a_missing_or_damaged_supervisor_record_never_fabricates_a_terminal_state() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let session = uuid::Uuid::new_v4().to_string();
        let task = super::super::tasks::execute_for_shell_owner(
            "TaskCreate",
            json!({"subject":"record probe","metadata":{
                "managed_shell":true,
                "interaction":INTERACTION
            }}),
            &root,
            &session,
        )
        .unwrap();
        let id = task["id"].as_str().unwrap().to_owned();
        super::super::tasks::execute_for_shell_owner(
            "TaskUpdate",
            json!({"id":&id,"status":"running"}),
            &root,
            &session,
        )
        .unwrap();
        let task_directory = root.join("shell-tasks").join(&session).join(&id);
        std::fs::create_dir_all(&task_directory).unwrap();
        // Missing: the historical presentation is unchanged, and nothing is
        // reported as finished.
        let missing = super::super::tasks::execute_for_session(
            "TaskGet",
            json!({"task_id":&id,"block":false}),
            &root,
            &session,
        )
        .unwrap();
        assert_eq!(missing["status"], "unresponsive", "{missing}");
        assert_eq!(missing["retrieval_status"], "not_ready", "{missing}");
        assert!(missing["owner_loss"].is_null(), "{missing}");
        // Damaged: a bounded, explicit error instead of a fabricated status.
        std::fs::write(
            task_directory.join("supervisor-state.json"),
            b"{\"owner_lost\":",
        )
        .unwrap();
        let error = super::super::tasks::execute_for_session(
            "TaskGet",
            json!({"task_id":&id,"block":false}),
            &root,
            &session,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("supervisor-state.json"),
            "the damaged record was not reported: {message}"
        );
        let stored = super::super::tasks::read_for_session(&id, &root, &session).unwrap();
        assert_eq!(stored["status"], "running", "{stored}");

        // A pipe task is not a PTY task: the same file must be ignored, not
        // parsed, so an unrelated same-named file cannot disturb the query.
        let pipe_session = uuid::Uuid::new_v4().to_string();
        let pipe = super::super::tasks::execute_for_shell_owner(
            "TaskCreate",
            json!({"subject":"pipe probe","metadata":{
                "managed_shell":true,
                "interaction":"pipe"
            }}),
            &root,
            &pipe_session,
        )
        .unwrap();
        let pipe_id = pipe["id"].as_str().unwrap().to_owned();
        super::super::tasks::execute_for_shell_owner(
            "TaskUpdate",
            json!({"id":&pipe_id,"status":"running"}),
            &root,
            &pipe_session,
        )
        .unwrap();
        let pipe_directory = root.join("shell-tasks").join(&pipe_session).join(&pipe_id);
        std::fs::create_dir_all(&pipe_directory).unwrap();
        std::fs::write(
            pipe_directory.join("supervisor-state.json"),
            b"{\"owner_lost\":",
        )
        .unwrap();
        let pipe_value = super::super::tasks::execute_for_session(
            "TaskGet",
            json!({"task_id":&pipe_id,"block":false}),
            &root,
            &pipe_session,
        )
        .unwrap();
        assert_eq!(pipe_value["status"], "unresponsive", "{pipe_value}");
        assert!(pipe_value["owner_loss"].is_null(), "{pipe_value}");
        let pipe_stored =
            super::super::tasks::read_for_session(&pipe_id, &root, &pipe_session).unwrap();
        assert_eq!(pipe_stored["status"], "running", "{pipe_stored}");
    }

    #[test]
    fn a_cleanup_failure_never_reports_a_clean_terminal() {
        // The status decision is pure; end-to-end failure injection lives in
        // the tests that exercise the real sink and persistence paths.
        assert_eq!(terminal_status(true, false, true, Some(0)), "failed");
        assert_eq!(terminal_status(false, false, true, Some(0)), "failed");
        assert_eq!(terminal_status(true, true, false, Some(0)), "failed");
        assert_eq!(terminal_status(false, true, false, Some(0)), "failed");
        assert_eq!(terminal_status(true, true, true, Some(0)), "cancelled");
        assert_eq!(terminal_status(false, true, true, Some(0)), "completed");
        assert_eq!(terminal_status(false, true, true, Some(2)), "failed");
        assert_eq!(terminal_status(false, true, true, None), "failed");
    }

    #[test]
    fn startup_registers_ownership_before_the_owner_thread_starts() {
        use std::sync::atomic::AtomicBool;

        let observed = std::sync::Arc::new(AtomicBool::new(false));
        let registered = std::sync::Arc::new(AtomicBool::new(false));
        let saw_active = observed.clone();
        let saw_registration = registered.clone();
        startup_probe::install(Box::new(move |id| {
            saw_registration.store(registration_probe::is_registered(id), Ordering::Release);
            saw_active.store(
                super::super::background_shell::has_active(),
                Ordering::Release,
            );
        }));
        let _clear = ClearStartupProbe;

        let mut fixture = Fixture::start("printf 'READY\\n'; sleep 30");
        assert!(
            observed.load(Ordering::Acquire),
            "has_active() was false at the startup barrier"
        );
        assert!(
            registered.load(Ordering::Acquire),
            "the task was not registered at the startup barrier"
        );
        assert!(registration_probe::is_registered(&fixture.id));
        let _ = fixture.stop_and_wait();
        assert!(
            !registration_probe::is_registered(&fixture.id),
            "registration survived final cleanup"
        );
    }

    #[test]
    fn failed_spawn_rolls_back_registration_and_cleans_the_child() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let session = uuid::Uuid::new_v4().to_string();
        let script = "printf 'READY\\n'; sleep 30";
        startup_injection::fail_next_spawn();
        let error = match super::start(
            spec_with_cwd(script, &root),
            &root,
            &session,
            "spawn failure",
            script,
        ) {
            Ok(_) => panic!("the injected spawn failure started a task"),
            Err(error) => error,
        };
        let message = format!("{error:#}");
        assert!(
            message.contains("Spawning the managed PTY owner thread"),
            "{message}"
        );
        let task_root = root.join("shell-tasks").join(&session);
        let ids: Vec<String> = std::fs::read_dir(&task_root)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(ids.len(), 1, "{ids:?}");
        let id = &ids[0];
        assert!(
            !registration_probe::is_registered(id),
            "the failed spawn left the task registered"
        );
        let record = super::super::tasks::read_for_session(id, &root, &session).unwrap();
        assert_eq!(record["status"], "failed", "{record}");
        let directory = task_root.join(id);
        let state: Value = serde_json::from_str(
            &std::fs::read_to_string(directory.join("terminal-state.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(state["startup_failed"], true, "{state}");
        assert_eq!(state["cleanup_complete"], true, "{state}");
        let pid = state["terminal_pid"].as_u64().unwrap() as u32;
        assert!(
            !session_has_live_members(pid),
            "the failed startup left a live session member"
        );
        assert!(
            !Path::new(&format!("/proc/{pid}")).exists(),
            "the failed startup left its child process"
        );
        let descriptor: Value = serde_json::from_str(
            &std::fs::read_to_string(directory.join("terminal.json")).unwrap(),
        )
        .unwrap();
        assert!(
            !Path::new(descriptor["endpoint"].as_str().unwrap()).exists(),
            "the failed startup left its endpoint socket"
        );
    }

    #[test]
    fn owner_guard_stop_cancels_a_blocked_write() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let gate = root.join("gate");
        let consumed = root.join("consumed.bin");
        let script = blocked_writer_script(&gate, &consumed);
        let mut fixture = Fixture::from_spec(directory, spec_with_cwd(&script, &root), &script);
        fixture.wait_output("READY");

        let (first_id, mut first) = start_blocked_write(&fixture, 60_000);
        let (_, mut second) = queue_write(&fixture, "SECOND-MARKER\n", 60_000);
        // The partial notification proves progress only; the final accepted
        // count is read after the owner finished, because the cancelled write
        // may accept a bounded number of further bytes before it observes the
        // flag.
        let partial = claim_probe::accepted_bytes(&first_id);
        assert!(
            partial > 0 && partial < MAX_DATA_BYTES,
            "the blocked write never made partial progress: {partial} bytes"
        );

        let started = Instant::now();
        let state = fixture.stop_and_wait();
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(10),
            "guard stop took {elapsed:?}, not comfortably below the 60 s write budget"
        );

        let first_reply = read_reply(&mut first);
        let accepted = claim_probe::accepted_bytes(&first_id);
        assert_eq!(first_reply["ok"], true, "{first_reply}");
        assert_eq!(first_reply["cancelled"], true, "{first_reply}");
        // Cancellation, not the request deadline and not terminal closure,
        // ended the write; these flags distinguish the three paths.
        assert_eq!(first_reply["timed_out"], false, "{first_reply}");
        assert_eq!(first_reply["closed"], false, "{first_reply}");
        assert_eq!(first_reply["write_complete"], false, "{first_reply}");
        assert_eq!(
            first_reply["bytes_written"].as_u64().unwrap() as usize,
            accepted,
            "the reported prefix must equal the bytes the transport accepted: {first_reply}"
        );
        // `partial` was sampled before the guard asked for cancellation; the
        // difference up to `accepted` is the asynchronous observation window
        // (marker poll plus one transport slice), not a write after
        // cancellation was observed. The reply and the final probe count are
        // the authoritative prefix.
        assert_eq!(
            first_reply["bytes_remaining"].as_u64().unwrap() as usize,
            MAX_DATA_BYTES - accepted,
            "{first_reply}"
        );
        assert!(
            first_reply["bytes_remaining"].as_u64().unwrap() > 0,
            "{first_reply}"
        );
        let second_reply = read_reply(&mut second);
        assert_eq!(second_reply["outcome"], "not_executed", "{second_reply}");
        assert_eq!(
            second_reply["may_have_been_written"], false,
            "{second_reply}"
        );

        assert_eq!(state["status"], "cancelled", "{state}");
        assert_eq!(state["cleanup_complete"], true, "{state}");
        assert_eq!(state["output_capture_complete"], true, "{state}");
        assert_eq!(state["final_drain_complete"], true, "{state}");
        assert_eq!(state["terminal_state_persisted"], true, "{state}");
        assert_eq!(state["task_record_updated"], true, "{state}");
        let endpoint = fixture.endpoint();
        assert!(!endpoint.exists(), "the endpoint survived shutdown");
        assert!(
            !session_has_live_members(fixture.child_pid),
            "the owned session still has live members"
        );
        assert!(
            !Path::new(&format!("/proc/{}", fixture.child_pid)).exists(),
            "the child process still exists"
        );
        assert!(
            UnixStream::connect(&endpoint).is_err(),
            "a new connection succeeded after shutdown"
        );
        assert_eq!(
            claim_probe::accepted_bytes(&first_id),
            accepted,
            "the cancelled write produced more bytes after shutdown"
        );
        assert!(claim_probe::is_finished(&first_id));
    }

    #[test]
    fn task_stop_marker_cancels_a_blocked_write() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let gate = root.join("gate");
        let consumed = root.join("consumed.bin");
        let script = blocked_writer_script(&gate, &consumed);
        let mut fixture = Fixture::from_spec(directory, spec_with_cwd(&script, &root), &script);
        fixture.wait_output("READY");

        let (_first_id, mut first) = start_blocked_write(&fixture, 60_000);
        let stopped = super::super::tasks::execute_for_session(
            "TaskStop",
            json!({"task_id":fixture.id}),
            &fixture.root,
            &fixture.session,
        )
        .unwrap();
        assert_eq!(stopped["cancellation_requested"], true, "{stopped}");
        let started = Instant::now();
        let reply = read_reply(&mut first);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "TaskStop took {:?} to reach the blocked write",
            started.elapsed()
        );
        assert_eq!(reply["cancelled"], true, "{reply}");
        assert!(reply["bytes_written"].as_u64().unwrap() > 0, "{reply}");
        assert!(reply["bytes_remaining"].as_u64().unwrap() > 0, "{reply}");
        let state = wait_for(
            "the terminal state",
            Instant::now() + Duration::from_secs(10),
            || fixture.terminal_state(),
        );
        assert_eq!(state["status"], "cancelled", "{state}");
        assert_eq!(state["cleanup_complete"], true, "{state}");
        assert_eq!(state["output_capture_complete"], true, "{state}");
        fixture
            .guard
            .as_mut()
            .unwrap()
            .join(Instant::now() + Duration::from_secs(10))
            .unwrap();
        assert!(!session_has_live_members(fixture.child_pid));
    }

    #[test]
    fn endpoint_stop_cancels_a_blocked_write() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let gate = root.join("gate");
        let consumed = root.join("consumed.bin");
        let script = blocked_writer_script(&gate, &consumed);
        let mut fixture = Fixture::from_spec(directory, spec_with_cwd(&script, &root), &script);
        fixture.wait_output("READY");

        let (first_id, mut first) = start_blocked_write(&fixture, 60_000);
        let partial = claim_probe::accepted_bytes(&first_id);
        assert!(partial > 0 && partial < MAX_DATA_BYTES);
        let stop_id = uuid::Uuid::new_v4().to_string();
        let mut stop_stream = UnixStream::connect(fixture.endpoint()).unwrap();
        stop_stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let mut request = envelope_with(&fixture, "stop", &stop_id);
        request["timeout_ms"] = json!(10_000);
        let started = Instant::now();
        send_frame(&mut stop_stream, &request);

        let active = read_reply(&mut first);
        // The active reply is sent only after the transport stopped writing, so
        // the probe count read now is the confirmed prefix. The difference from
        // the pre-stop sample is the asynchronous observation window.
        let accepted = claim_probe::accepted_bytes(&first_id);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the endpoint stop took {:?} to reach the blocked write",
            started.elapsed()
        );
        assert_eq!(active["cancelled"], true, "{active}");
        assert_eq!(active["timed_out"], false, "{active}");
        assert_eq!(active["closed"], false, "{active}");
        assert_eq!(
            active["bytes_written"].as_u64().unwrap() as usize,
            accepted,
            "{active}"
        );
        assert_eq!(
            active["bytes_remaining"].as_u64().unwrap() as usize,
            MAX_DATA_BYTES - accepted,
            "{active}"
        );
        let stopped = read_reply(&mut stop_stream);
        assert_eq!(stopped["ok"], true, "{stopped}");
        assert_eq!(stopped["cancellation_requested"], true, "{stopped}");
        assert_eq!(stopped["confirmed"], false, "{stopped}");
        // The active reply is the finished barrier: no further byte may be
        // accepted after it, so the confirmed prefix stays the same.
        assert_eq!(
            claim_probe::accepted_bytes(&first_id),
            accepted,
            "the cancelled write accepted bytes after it reported completion"
        );

        let state = wait_for(
            "the terminal state",
            Instant::now() + Duration::from_secs(10),
            || fixture.terminal_state(),
        );
        assert_eq!(state["status"], "cancelled", "{state}");
        assert_eq!(state["cleanup_complete"], true, "{state}");
        assert_eq!(state["output_capture_complete"], true, "{state}");
        assert!(!session_has_live_members(fixture.child_pid));
    }

    #[test]
    fn peer_loss_cancels_further_bytes_without_replay() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let gate = root.join("gate");
        let count = root.join("count");
        let consumed = root.join("consumed");
        let extra = root.join("extra");
        std::fs::write(&extra, b"").unwrap();
        let script = format!(
            "stty raw -echo\n\
             printf 'READY\\n'\n\
             while [ ! -e '{gate}' ]; do sleep 0.01; done\n\
             n=$(cat '{count}')\n\
             dd if=/dev/stdin of='{consumed}' bs=1 count=$n 2>/dev/null\n\
             dd if=/dev/stdin of='{extra}' bs=1 count=1 iflag=nonblock 2>/dev/null\n\
             printf 'EXTRA:%s\\n' \"$(wc -c < '{extra}')\"\n\
             while read -r line; do printf 'GOT:%s\\n' \"$line\"; done\n",
            gate = gate.display(),
            count = count.display(),
            consumed = consumed.display(),
            extra = extra.display()
        );
        let fixture = Fixture::from_spec(directory, spec_with_cwd(&script, &root), &script);
        fixture.wait_output("READY");

        let (request_id, stream) = start_blocked_write(&fixture, 60_000);
        drop(stream);
        assert!(
            claim_probe::wait_finished(&request_id, Instant::now() + Duration::from_secs(10)),
            "peer loss did not cancel the blocked write within 10 s"
        );
        // The final count is authoritative: the write may accept a bounded
        // number of further bytes between the partial notification and the
        // cancellation. Counting after completion is what proves no byte was
        // written *after* cancellation.
        let accepted = claim_probe::accepted_bytes(&request_id);
        assert!(
            accepted > 0 && accepted < MAX_DATA_BYTES,
            "peer loss did not leave a partial write: {accepted} bytes"
        );
        std::fs::write(&count, accepted.to_string()).unwrap();
        std::fs::write(&gate, b"go").unwrap();
        let log = wait_for("the extra-byte probe", Instant::now() + BUDGET, || {
            let log = fixture.stdout();
            log.contains("EXTRA:").then_some(log)
        });
        assert!(
            log.contains("EXTRA:0"),
            "a delayed write produced extra bytes after peer loss: {log}"
        );
        let bytes = std::fs::read(&consumed).unwrap();
        assert_eq!(
            bytes.len(),
            accepted,
            "the child consumed a different prefix"
        );
        assert_eq!(
            bytes,
            vec![b'a'; accepted],
            "the child did not receive the exact accepted prefix"
        );
        assert!(
            log.contains("EXTRA:0"),
            "a delayed write produced extra bytes after peer loss: {log}"
        );
        // The finished barrier was reached before the child was released, so a
        // later probe read proves the cancelled write produced nothing more.
        assert_eq!(
            claim_probe::accepted_bytes(&request_id),
            accepted,
            "the cancelled write produced bytes after it reported completion"
        );

        let reply = fixture.call(json!({"data":"after-peer-loss\n"}));
        assert_eq!(reply["ok"], true, "{reply}");
        fixture.wait_output("GOT:after-peer-loss");
    }

    /// Endpoint *path* loss (the socket file is removed) is not peer
    /// disconnect. The owner holds its bound inode and cannot observe a path
    /// removal, so the only truthful statement is: losing the path does not
    /// undo accepted bytes, it does not hang, and cancellation still works
    /// through the documented marker/guard path rather than through a new
    /// connection.
    #[test]
    fn endpoint_path_loss_keeps_other_cancellation_paths_working() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let gate = root.join("gate");
        let consumed = root.join("consumed.bin");
        let script = blocked_writer_script(&gate, &consumed);
        let mut fixture = Fixture::from_spec(directory, spec_with_cwd(&script, &root), &script);
        fixture.wait_output("READY");

        let (first_id, mut first) = start_blocked_write(&fixture, 60_000);
        let partial = claim_probe::accepted_bytes(&first_id);
        assert!(partial > 0 && partial < MAX_DATA_BYTES);

        std::fs::remove_file(fixture.endpoint()).unwrap();
        // A new control call fails cleanly and quickly: no retry loop and no
        // silent success through a removed path.
        let started = Instant::now();
        let error = super::execute(
            &json!({"task_id":fixture.id,"data":"NOOP\n"}),
            &fixture.root,
            &fixture.session,
            None,
        )
        .unwrap_err();
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a call after endpoint path loss took {:?}",
            started.elapsed()
        );
        let message = format!("{error:#}");
        assert!(
            message.contains("endpoint")
                || message.contains("No such file")
                || message.contains("terminal descriptor"),
            "{message}"
        );
        // Path loss alone is not a cancellation source.
        assert!(
            !claim_probe::is_finished(&first_id),
            "the blocked write ended without any control path cancelling it"
        );

        // The existing marker path still reaches the owner, and the accepted
        // prefix is preserved and reported truthfully.
        let stopped = super::super::tasks::execute_for_session(
            "TaskStop",
            json!({"task_id":fixture.id}),
            &fixture.root,
            &fixture.session,
        )
        .unwrap();
        assert_eq!(stopped["cancellation_requested"], true, "{stopped}");
        let active = read_reply(&mut first);
        let accepted = claim_probe::accepted_bytes(&first_id);
        assert_eq!(active["cancelled"], true, "{active}");
        assert_eq!(
            active["bytes_written"].as_u64().unwrap() as usize,
            accepted,
            "{active}"
        );
        assert!(accepted >= partial, "{active}");
        assert!(active["bytes_remaining"].as_u64().unwrap() > 0, "{active}");

        let state = wait_for(
            "the terminal state",
            Instant::now() + Duration::from_secs(10),
            || fixture.terminal_state(),
        );
        assert_eq!(state["status"], "cancelled", "{state}");
        assert_eq!(state["cleanup_complete"], true, "{state}");
        assert!(!session_has_live_members(fixture.child_pid));
    }

    /// The accept thread must keep admitting control requests when ordinary
    /// work already fills `MAX_PENDING`. Before the fix, `pending.len() ==
    /// MAX_PENDING` skipped `accept()` entirely, so a Stop waited in the
    /// backlog while the blocked write ran until its own 60 s budget.
    #[test]
    fn a_stop_is_admitted_while_pending_writes_saturate_the_queue() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let gate = root.join("gate");
        let consumed = root.join("consumed.bin");
        let script = blocked_writer_script(&gate, &consumed);
        let mut fixture = Fixture::from_spec(directory, spec_with_cwd(&script, &root), &script);
        fixture.wait_output("READY");

        let (first_id, mut first) = start_blocked_write(&fixture, 60_000);
        let mut queued = Vec::new();
        for index in 0..MAX_PENDING - 1 {
            queued.push(queue_write(&fixture, &format!("QUEUED-{index}\n"), 60_000));
        }

        // Capacity is full: one claimed blocked write plus three queued ones.
        // An ordinary request beyond capacity is refused explicitly and does
        // not consume a slot or the backlog.
        let refused_id = uuid::Uuid::new_v4().to_string();
        let mut refused = UnixStream::connect(fixture.endpoint()).unwrap();
        refused
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut request = envelope_with(&fixture, "write", &refused_id);
        request["data"] = json!("OVER-CAPACITY\n");
        request["timeout_ms"] = json!(10_000);
        let refused_started = Instant::now();
        send_frame(&mut refused, &request);
        let refused_reply = read_reply_bounded(&mut refused, Duration::from_secs(10))
            .expect("an over-capacity request was neither admitted nor explicitly refused");
        assert!(
            refused_started.elapsed() < Duration::from_secs(10),
            "an over-capacity request waited instead of being refused"
        );
        assert_eq!(refused_reply["outcome"], "not_executed", "{refused_reply}");
        assert_eq!(
            refused_reply["may_have_been_written"], false,
            "{refused_reply}"
        );

        // Control is still admitted at the bound.
        let stop_id = uuid::Uuid::new_v4().to_string();
        let mut stop_stream = UnixStream::connect(fixture.endpoint()).unwrap();
        stop_stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut stop = envelope_with(&fixture, "stop", &stop_id);
        stop["timeout_ms"] = json!(10_000);
        let started = Instant::now();
        send_frame(&mut stop_stream, &stop);
        let stop_reply = read_reply_bounded(&mut stop_stream, Duration::from_secs(10)).expect(
            "the Stop was never admitted: the accept thread skipped accept() while the queue was full",
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "the Stop was not admitted while the queue was full: {:?}",
            started.elapsed()
        );
        assert_eq!(stop_reply["cancellation_requested"], true, "{stop_reply}");
        assert_eq!(stop_reply["confirmed"], false, "{stop_reply}");

        let active = read_reply(&mut first);
        let accepted = claim_probe::accepted_bytes(&first_id);
        assert_eq!(active["cancelled"], true, "{active}");
        assert_eq!(
            active["bytes_written"].as_u64().unwrap() as usize,
            accepted,
            "{active}"
        );
        assert!(accepted > 0 && accepted < MAX_DATA_BYTES, "{active}");
        assert!(active["bytes_remaining"].as_u64().unwrap() > 0, "{active}");
        for (_, mut stream) in queued {
            let reply = read_reply(&mut stream);
            assert_eq!(reply["outcome"], "not_executed", "{reply}");
            assert_eq!(reply["may_have_been_written"], false, "{reply}");
        }

        let state = wait_for(
            "the terminal state",
            Instant::now() + Duration::from_secs(10),
            || fixture.terminal_state(),
        );
        assert_eq!(state["status"], "cancelled", "{state}");
        assert_eq!(state["cleanup_complete"], true, "{state}");
        assert!(!session_has_live_members(fixture.child_pid));
    }

    #[test]
    fn an_expired_stop_has_no_side_effect() {
        let _serial = gate_serial();
        let mut fixture = Fixture::start(
            "stty -echo; printf 'READY\\n'; while read -r line; do printf 'GOT:%s\\n' \"$line\"; done",
        );
        fixture.wait_output("READY");
        let _clear = ClearGates;

        let stop_id = uuid::Uuid::new_v4().to_string();
        admit_gate::arm(&fixture.id, &stop_id);
        let mut stream = UnixStream::connect(fixture.endpoint()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        let mut request = envelope_with(&fixture, "stop", &stop_id);
        request["timeout_ms"] = json!(150);
        send_frame(&mut stream, &request);
        assert!(
            admit_gate::entered(Instant::now() + BUDGET),
            "the accept thread never reached the admission barrier"
        );
        std::thread::sleep(Duration::from_millis(250));
        admit_gate::open();

        let reply = read_reply(&mut stream);
        assert_eq!(reply["outcome"], "not_executed", "{reply}");
        assert_eq!(reply["may_have_been_applied"], false, "{reply}");
        assert!(
            fixture.terminal_state().is_none(),
            "the task terminated despite an expired stop"
        );

        // No cancellation side effect: a later write is still accepted.
        let accepted = fixture.call(json!({"data":"after-expired-stop\n"}));
        assert_eq!(accepted["ok"], true, "{accepted}");
        fixture.wait_output("GOT:after-expired-stop");

        let stopped = super::super::tasks::execute_for_session(
            "TaskStop",
            json!({"task_id":fixture.id}),
            &fixture.root,
            &fixture.session,
        )
        .unwrap();
        assert_eq!(stopped["cancellation_requested"], true, "{stopped}");
        let state = wait_for("the terminal state", Instant::now() + BUDGET, || {
            fixture.terminal_state()
        });
        assert_eq!(state["status"], "cancelled", "{state}");
        assert_eq!(state["cleanup_complete"], true, "{state}");
    }

    #[test]
    fn final_metadata_write_failure_is_reported_by_the_guard() {
        let mut fixture = Fixture::start("printf 'READY\\n'; sleep 30");
        fixture.wait_output("READY");
        // A real filesystem fault: the atomic rename must fail because the
        // destination is a directory.
        std::fs::create_dir(fixture.directory().join("terminal-state.json")).unwrap();
        let error = fixture
            .guard
            .as_mut()
            .unwrap()
            .stop_and_join(Instant::now() + BUDGET)
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("terminal-state persisted: false"),
            "{message}"
        );
        assert!(!session_has_live_members(fixture.child_pid));
        let record =
            super::super::tasks::read_for_session(&fixture.id, &fixture.root, &fixture.session)
                .unwrap();
        assert_eq!(record["status"], "failed", "{record}");
    }

    #[test]
    fn task_record_failure_is_reported_as_a_guard_failure() {
        let mut fixture = Fixture::start("printf 'READY\\n'; sleep 30");
        fixture.wait_output("READY");
        fixture.guard.as_ref().unwrap().fail_task_update();
        let error = fixture
            .guard
            .as_mut()
            .unwrap()
            .stop_and_join(Instant::now() + BUDGET)
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("cleanup_complete: true"), "{message}");
        assert!(
            message.contains("output_capture_complete: true"),
            "{message}"
        );
        assert!(message.contains("task record updated: false"), "{message}");
        assert!(
            message.contains("injected task record update failure"),
            "{message}"
        );
        let state = fixture.terminal_state().unwrap();
        assert_eq!(state["task_record_updated"], false, "{state}");
        assert!(
            state["task_record_error"]
                .as_str()
                .unwrap()
                .contains("injected"),
            "{state}"
        );
        let record =
            super::super::tasks::read_for_session(&fixture.id, &fixture.root, &fixture.session)
                .unwrap();
        assert_eq!(record["status"], "running", "{record}");
        assert!(!session_has_live_members(fixture.child_pid));
    }

    #[test]
    fn final_state_and_task_record_failure_is_not_success() {
        let mut fixture = Fixture::start("printf 'READY\\n'; sleep 30");
        fixture.wait_output("READY");
        std::fs::create_dir(fixture.directory().join("terminal-state.json")).unwrap();
        fixture.guard.as_ref().unwrap().fail_task_update();
        let error = fixture
            .guard
            .as_mut()
            .unwrap()
            .stop_and_join(Instant::now() + BUDGET)
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("terminal-state persisted: false"),
            "{message}"
        );
        assert!(message.contains("task record updated: false"), "{message}");
        assert!(
            message.contains("injected task record update failure"),
            "{message}"
        );
        assert!(!session_has_live_members(fixture.child_pid));
    }

    #[test]
    fn final_drain_sink_failure_is_an_output_failure_not_a_cleanup_failure() {
        let mut fixture = Fixture::start("printf 'READY\\n'; sleep 30");
        fixture.wait_output("READY");
        fixture
            .guard
            .as_ref()
            .unwrap()
            .inject_final_tail(b"FINAL-TAIL-MARKER\n");
        fixture.guard.as_ref().unwrap().inject_output_fault(1);
        let state = fixture.stop_and_wait();
        assert_eq!(state["status"], "failed", "{state}");
        assert_eq!(state["cleanup_complete"], true, "{state}");
        assert_eq!(state["output_capture_complete"], false, "{state}");
        assert!(
            state["output_error"].as_str().unwrap().contains("injected"),
            "{state}"
        );
        assert_eq!(state["final_drain_complete"], false, "{state}");
        assert!(
            !fixture.stdout().contains("FINAL-TAIL-MARKER"),
            "a failed write still published the tail"
        );
        let record =
            super::super::tasks::read_for_session(&fixture.id, &fixture.root, &fixture.session)
                .unwrap();
        assert_eq!(record["status"], "failed", "{record}");
        assert!(!session_has_live_members(fixture.child_pid));
    }

    #[test]
    fn final_flush_failure_is_reported() {
        let mut fixture = Fixture::start("printf 'READY\\n'; sleep 30");
        fixture.wait_output("READY");
        fixture
            .guard
            .as_ref()
            .unwrap()
            .inject_final_tail(b"FINAL-FLUSH-TAIL\n");
        fixture.guard.as_ref().unwrap().inject_output_fault(2);
        let state = fixture.stop_and_wait();
        assert_eq!(state["status"], "failed", "{state}");
        assert_eq!(state["cleanup_complete"], true, "{state}");
        assert_eq!(state["output_capture_complete"], false, "{state}");
        assert!(
            state["output_error"].as_str().unwrap().contains("flush"),
            "{state}"
        );
        assert_eq!(state["final_drain_complete"], false, "{state}");
        assert!(
            fixture.stdout().contains("FINAL-FLUSH-TAIL"),
            "the write succeeded before the flush failure, so the bytes are present"
        );
    }

    #[test]
    fn final_drain_captures_real_queued_terminal_bytes_once() {
        let _serial = gate_serial();
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let burst = root.join("burst");
        let started_marker = root.join("bursting");
        let script = format!(
            "stty raw -echo\n\
             printf 'READY\\n'\n\
             while [ ! -e '{}' ]; do sleep 0.01; done\n\
             touch '{}'\n\
             while :; do printf 'BURST-LINE\\n'; done\n",
            burst.display(),
            started_marker.display()
        );
        let mut fixture = Fixture::from_spec(directory, spec_with_cwd(&script, &root), &script);
        fixture.wait_output("READY");
        let _clear = ClearGates;
        pre_stop_gate::arm(&fixture.id);

        let mut guard = fixture.guard.take().unwrap();
        let stdout_path = fixture.directory().join("stdout.log");
        let terminal_state = fixture.directory().join("terminal-state.json");
        let burst_path = burst.clone();
        let probe_path = started_marker.clone();
        let stdout_read = stdout_path.clone();
        let (state, before_len) = std::thread::scope(|scope| {
            let stopper = scope.spawn(move || guard.stop_and_join(Instant::now() + BUDGET));
            assert!(
                pre_stop_gate::entered(Instant::now() + BUDGET),
                "the owner never reached the pre-stop barrier"
            );
            std::fs::write(&burst_path, b"go").unwrap();
            // The child is now writing and the owner is not reading; wait for
            // proof that the child started, then let its queue fill.
            wait_for(
                "the child to start writing",
                Instant::now() + BUDGET,
                || probe_path.exists().then_some(()),
            );
            std::thread::sleep(Duration::from_millis(250));
            let before = std::fs::read_to_string(&stdout_read)
                .unwrap_or_default()
                .len();
            pre_stop_gate::open();
            let state = stopper
                .join()
                .unwrap()
                .expect("the owner finalizes with persisted metadata");
            (state, before)
        });

        let after = std::fs::read_to_string(&stdout_path).unwrap_or_default();
        assert!(
            after.len() > before_len,
            "the final drain appended no real queued bytes (before {before_len}, after {})",
            after.len()
        );
        assert!(
            after.contains("BURST-LINE"),
            "the queued terminal burst was not captured"
        );
        assert_eq!(state["status"], "cancelled", "{state}");
        assert_eq!(state["cleanup_complete"], true, "{state}");
        assert_eq!(state["final_drain_complete"], true, "{state}");
        assert_eq!(state["output_capture_complete"], true, "{state}");
        assert!(terminal_state.exists());
        assert_eq!(
            fixture.paged_stdout(),
            after,
            "paging lost, reordered or duplicated the buffered tail"
        );
        assert!(!session_has_live_members(fixture.child_pid));
    }

    #[test]
    fn an_expired_resize_is_not_applied_after_the_owner_resumes() {
        let _serial = gate_serial();
        let fixture = Fixture::start(
            "stty -echo; printf 'READY\\n'; while read -r line; do case \"$line\" in size) stty size;; *) printf 'GOT:%s\\n' \"$line\";; esac; done",
        );
        fixture.wait_output("READY");
        let _clear = ClearGates;

        let request_id = uuid::Uuid::new_v4().to_string();
        serve_gate::arm(&request_id);
        scan_gate::arm(&fixture.id);
        let mut stream = UnixStream::connect(fixture.endpoint()).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        let mut request = envelope_with(&fixture, "resize", &request_id);
        request["rows"] = json!(40);
        request["cols"] = json!(120);
        request["timeout_ms"] = json!(150);
        send_frame(&mut stream, &request);
        assert!(
            serve_gate::entered(Instant::now() + BUDGET),
            "the owner never reached the claim barrier"
        );
        assert!(
            scan_gate::entered(Instant::now() + BUDGET),
            "the accept thread never reached the scan barrier"
        );
        std::thread::sleep(Duration::from_millis(250));
        serve_gate::open();
        assert!(
            claim_probe::wait_finished(&request_id, Instant::now() + BUDGET),
            "the owner never finished the expired resize"
        );
        scan_gate::open();

        let reply = read_reply(&mut stream);
        assert_eq!(reply["outcome"], "not_executed", "{reply}");
        assert_eq!(reply["may_have_been_applied"], false, "{reply}");

        // No late effect: the terminal still has its original size.
        fixture.call(json!({"data":"size\n"}));
        fixture.wait_output("24 80");
        let resized = fixture.call(json!({"resize":{"rows":40,"cols":120}}));
        assert_eq!(resized["rows"], 40, "{resized}");
        fixture.call(json!({"data":"size\n"}));
        fixture.wait_output("40 120");
    }

    #[test]
    fn final_state_cleanup_and_output_facts_stay_separate() {
        // A pure decision check: cleanup, output capture and the final tail are
        // independent, and a missing tail never becomes `completed`.
        assert_eq!(terminal_status(false, true, true, Some(0)), "completed");
        assert_eq!(terminal_status(false, true, false, Some(0)), "failed");
        assert_eq!(terminal_status(false, false, true, Some(0)), "failed");
        assert_eq!(terminal_status(true, true, true, Some(0)), "cancelled");
    }

    /// One `(pid, start tick)` identity pair from `/proc/<pid>/stat`.
    fn process_identity(pid: u32) -> Option<(u32, u64)> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let (_, rest) = stat.rsplit_once(')')?;
        let fields: Vec<&str> = rest.split_whitespace().collect();
        if fields.len() < 20 {
            return None;
        }
        let session: u32 = fields[3].parse().ok()?;
        let start: u64 = fields[19].parse().ok()?;
        Some((session, start))
    }

    /// SIGKILL exactly this process, but only while it is still the instance
    /// that was observed. Nothing is signalled by name or by an unverified PID.
    fn kill_exact(pid: u32, start: u64) {
        let Some(identity) = process_identity(pid) else {
            return;
        };
        if identity.1 != start {
            return;
        }
        let Some(pid_value) = rustix::process::Pid::from_raw(pid as i32) else {
            return;
        };
        if let Ok(pidfd) =
            rustix::process::pidfd_open(pid_value, rustix::process::PidfdFlags::empty())
        {
            let _ = rustix::process::pidfd_send_signal(&pidfd, rustix::process::Signal::KILL);
        }
    }

    /// Every live `(pid, creation tick)` in one session, read directly from
    /// `/proc`. Zombies are included because their next state change is reaping,
    /// not survival.
    fn session_instances(session: u32) -> Vec<(u32, u64)> {
        let mut pids = Vec::new();
        if let Ok(entries) = std::fs::read_dir("/proc") {
            for entry in entries.flatten() {
                let Some(pid) = entry
                    .file_name()
                    .to_str()
                    .and_then(|name| name.parse::<u32>().ok())
                else {
                    continue;
                };
                if let Some((sid, start)) = process_identity(pid) {
                    if sid == session {
                        pids.push((pid, start));
                    }
                }
            }
        }
        pids
    }

    /// True when the exact process instance `(pid, creation tick)` is present.
    ///
    /// A recycled PID is a different instance, so it never counts as survival.
    fn instance_alive(pid: u32, start: u64) -> bool {
        process_identity(pid).is_some_and(|(_, observed)| observed == start)
    }

    /// True when the exact instance is gone. This is deliberately not
    /// `!Path::exists`: a PID reused by an unrelated process has a different
    /// creation tick and must not be mistaken for the original member.
    fn instance_gone(pid: u32, start: u64) -> bool {
        !instance_alive(pid, start)
    }

    fn process_state(pid: u32) -> Option<char> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let (_, rest) = stat.rsplit_once(')')?;
        rest.split_whitespace().next()?.chars().next()
    }

    /// True when the exact instance has exited. A zombie has exited and is only
    /// waiting to be reaped, so it counts here but not in [`instance_alive`].
    fn instance_exited(pid: u32, start: u64) -> bool {
        match process_identity(pid) {
            None => true,
            Some((_, observed)) if observed != start => true,
            Some(_) => process_state(pid).is_some_and(|state| matches!(state, 'Z' | 'X' | 'x')),
        }
    }

    /// Exact-PID cleanup that also runs when an assertion panics, so a failing
    /// test never leaves its own terminal session behind for the next run.
    struct ExactCleanup {
        pids: Vec<(u32, u64)>,
    }

    impl ExactCleanup {
        fn capture(session: u32) -> Self {
            Self {
                pids: session_instances(session),
            }
        }

        fn add(&mut self, pid: u32) {
            if let Some((_, start)) = process_identity(pid) {
                self.pids.push((pid, start));
            }
        }
    }

    impl Drop for ExactCleanup {
        fn drop(&mut self) {
            for (pid, start) in self.pids.drain(..) {
                kill_exact(pid, start);
            }
        }
    }

    /// The helper side of [`owner_sigkill_cleans_the_whole_terminal_session`]:
    /// this process is the owner. It starts a task whose shell keeps a
    /// job-control descendant alive, publishes its identity, and waits to be
    /// killed by the parent test.
    fn owner_death_helper(config: &str) {
        let config: Value = serde_json::from_str(config).unwrap();
        let root = PathBuf::from(config["root"].as_str().unwrap());
        let session = config["session"].as_str().unwrap().to_owned();
        let script = config["script"].as_str().unwrap();
        let spec = spec_with_cwd(script, &root);
        let (value, guard) = super::start(spec, &root, &session, "owner death", script).unwrap();
        std::fs::write(
            config["info"].as_str().unwrap(),
            json!({
                "task_id": value["task_id"],
                "terminal_pid": value["terminal_pid"],
                "owner_pid": std::process::id()
            })
            .to_string(),
        )
        .unwrap();
        // The owner process is the test subject: it must not run the guard.
        std::mem::forget(guard);
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }

    /// Requirement 1. The owner process is killed with `SIGKILL`, so no `Drop`
    /// runs anywhere in it. An independent supervisor must clean the whole
    /// job-control terminal session, an unrelated sentinel must survive, and
    /// the child must produce no late file effect.
    ///
    /// The fixture makes HUP ignored for the whole session and keeps the script
    /// from finishing, so only the supervisor's `SIGKILL` can end the members.
    /// That makes "which process ended first" irrelevant: every recorded member
    /// must be gone and every PID the supervisor claims to have signalled must
    /// have been a member whose exact instance is gone. A run where a member
    /// leaves on its own (for example the terminal hangup in the earlier
    /// baseline) is a different, equally valid cleanup path and is not forced
    /// into `cleaned_pids`.
    ///
    /// Before the supervisor existed this test failed: the leader and its
    /// background group stayed alive. That failure is retained as evidence in
    /// `round44-owner-death-prefix.log`.
    #[test]
    fn owner_sigkill_cleans_the_whole_terminal_session() {
        if let Ok(config) = std::env::var("DCL_PTY_OWNER_DEATH_HELPER") {
            owner_death_helper(&config);
            return;
        }
        use std::os::unix::process::CommandExt as _;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let session = uuid::Uuid::new_v4().to_string();
        let info = root.join("owner-info.json");
        let bg_file = root.join("bg.pid");
        let sub_file = root.join("sub.pid");
        let late = root.join("LATE-EFFECT");
        // `SIG_IGN` is inherited across fork and exec, so the whole fixture
        // session ignores the terminal hangup caused by the owner's death. No
        // member can end by itself between the SIGKILL and the supervisor's
        // sweep: the session leader is blocked on a foreground `sleep` that
        // also ignores HUP. Only the supervisor's SIGKILL can end them, which
        // makes this a controlled cleanup scenario instead of a timing race.
        let script = format!(
            "set -m\n\
             trap '' HUP\n\
             printf 'READY\\n'\n\
             sleep 300 &\n\
             echo $! > '{}'\n\
             ( sleep 4; echo late > '{}' ) &\n\
             echo $! > '{}'\n\
             sleep 300\n",
            bg_file.display(),
            late.display(),
            sub_file.display()
        );
        let config = json!({
            "root": root,
            "session": session,
            "script": script,
            "info": info,
        });

        struct Helper(std::process::Child);
        impl Drop for Helper {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut helper = Helper(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tools::pty_owner::tests::owner_sigkill_cleans_the_whole_terminal_session",
                    "--nocapture",
                ])
                .env("DCL_PTY_OWNER_DEATH_HELPER", config.to_string())
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );

        let started = wait_for("the owner task", Instant::now() + BUDGET, || {
            std::fs::read_to_string(&info)
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        });
        let owner_pid = started["owner_pid"].as_u64().unwrap() as u32;
        let task_id = started["task_id"].as_str().unwrap().to_owned();
        let leader = started["terminal_pid"].as_u64().unwrap() as u32;
        let task_directory = root.join("shell-tasks").join(&session).join(&task_id);
        // The supervisor is a separate process started before the gate: its own
        // PID differs from the owner's, and it records the owner identity it
        // will watch. The cleanup state written after the owner dies is
        // therefore evidence from outside the killed process.
        let supervisor_ready: Value = wait_for(
            "the supervisor ready record",
            Instant::now() + BUDGET,
            || {
                std::fs::read_to_string(task_directory.join("supervisor.ready"))
                    .ok()
                    .and_then(|text| serde_json::from_str(&text).ok())
            },
        );
        let supervisor_pid = supervisor_ready["supervisor_pid"].as_u64().unwrap() as u32;
        let (_, supervisor_start) =
            process_identity(supervisor_pid).expect("the supervisor is a separate live process");
        assert_ne!(
            supervisor_pid, owner_pid,
            "the supervisor is not an independent process"
        );
        assert_eq!(
            supervisor_ready["owner_pid"],
            json!(owner_pid),
            "{supervisor_ready}"
        );
        // Wait for the user program itself, not just the task record: READY is
        // printed only after GO was released and the gate execed the script.
        let ready = wait_for(
            "the user program READY line",
            Instant::now() + BUDGET,
            || {
                std::fs::read_to_string(task_directory.join("stdout.log"))
                    .ok()
                    .filter(|log| log.contains("READY"))
            },
        );
        assert!(ready.contains("READY"), "{ready}");
        let bg_pid = wait_for("the background job PID", Instant::now() + BUDGET, || {
            std::fs::read_to_string(&bg_file)
                .ok()
                .and_then(|text| text.trim().parse::<u32>().ok())
        });
        let sub_pid = wait_for("the subshell job PID", Instant::now() + BUDGET, || {
            std::fs::read_to_string(&sub_file)
                .ok()
                .and_then(|text| text.trim().parse::<u32>().ok())
        });

        // Record the exact instances before the owner dies. Every later
        // survival check uses PID plus creation tick, never a name or a bare
        // PID, and a reused PID therefore cannot masquerade as a survivor.
        let (leader_session, leader_start) =
            process_identity(leader).expect("the session leader is live");
        assert_eq!(
            leader_session, leader,
            "the terminal leader is not the session leader"
        );
        let (bg_session, bg_start) = process_identity(bg_pid).expect("the background job is live");
        let (sub_session, sub_start) = process_identity(sub_pid).expect("the subshell job is live");

        let mut cleanup = ExactCleanup::capture(leader);
        cleanup.add(leader);
        cleanup.add(bg_pid);
        cleanup.add(sub_pid);

        // The scenario really has job-control descendants in the owner's
        // session, not just a single process.
        assert_eq!(
            bg_session, leader,
            "the background job does not share the terminal session"
        );
        assert_eq!(
            sub_session, leader,
            "the subshell job does not share the terminal session"
        );
        assert!(instance_alive(bg_pid, bg_start));
        assert!(instance_alive(sub_pid, sub_start));
        assert!(session_has_live_members(leader));

        // An unrelated process in its own process group must survive cleanup.
        let mut sentinel = std::process::Command::new("sleep")
            .arg("300")
            .process_group(0)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let sentinel_pid = sentinel.id();
        let (sentinel_session, sentinel_start) =
            process_identity(sentinel_pid).expect("the sentinel is live");
        assert_ne!(
            sentinel_session, leader,
            "the sentinel is in the terminal session it is meant to control for"
        );
        cleanup.add(sentinel_pid);

        // The exact membership observed immediately before the owner dies. With
        // HUP ignored and a script that cannot finish, no member can leave or
        // be replaced before the supervisor's sweep. A PID the supervisor later
        // claims to have signalled must belong to this set.
        let members_before_kill = session_instances(leader);
        for pid in [leader, bg_pid, sub_pid] {
            assert!(
                members_before_kill
                    .iter()
                    .any(|(member, _)| *member == pid),
                "PID {pid} was not a member of the recorded terminal session: {members_before_kill:?}"
            );
        }

        // Kill only the owner process. Verify the PID first, then send
        // SIGKILL: no Drop, no finalizer and no task update can run in it.
        assert_eq!(helper.0.id(), owner_pid);
        let (_, owner_start) =
            process_identity(owner_pid).expect("the owner is live before its SIGKILL");
        eprintln!(
            "owner death fixture before SIGKILL: owner={owner_pid}(start {owner_start}) supervisor={supervisor_pid}(start {supervisor_start}) leader={leader}(start {leader_start}) background={bg_pid}(start {bg_start}) subshell={sub_pid}(start {sub_start}) sentinel={sentinel_pid}(start {sentinel_start}) members={members_before_kill:?}"
        );
        helper.0.kill().unwrap();
        let _ = helper.0.wait();

        wait_for(
            "the exact terminal session instances to disappear",
            Instant::now() + Duration::from_secs(10),
            || {
                (instance_gone(leader, leader_start)
                    && instance_gone(bg_pid, bg_start)
                    && instance_gone(sub_pid, sub_start))
                .then_some(())
            },
        );
        assert!(
            !session_has_live_members(leader),
            "the terminal session survived the owner's SIGKILL: leader {leader}, background {bg_pid}, subshell {sub_pid}"
        );
        // Independent proof per recorded instance: PID plus creation tick.
        // This does not trust the supervisor's `remaining_pids` field.
        assert!(
            instance_gone(leader, leader_start),
            "the session leader instance PID {leader} (start {leader_start}) survived cleanup"
        );
        assert!(
            instance_gone(bg_pid, bg_start),
            "the background job instance PID {bg_pid} (start {bg_start}) survived cleanup"
        );
        assert!(
            instance_gone(sub_pid, sub_start),
            "the subshell job instance PID {sub_pid} (start {sub_start}) survived cleanup"
        );
        assert!(
            !Path::new(&format!("/proc/{}", helper.0.id())).exists(),
            "the owner process was not reaped"
        );
        assert!(
            !Path::new(&format!("/proc/{owner_pid}")).exists(),
            "the owner process {owner_pid} still exists"
        );
        // The unrelated sentinel is untouched, and it is still the identical
        // instance, not a recycled PID.
        assert!(
            instance_alive(sentinel_pid, sentinel_start),
            "the supervisor killed or replaced a process outside its session"
        );
        let _ = sentinel.kill();
        let _ = sentinel.wait();

        // No late file effect: the deferred writer must never run. It is a
        // single sleeping subshell (no new processes after the pre-kill
        // snapshot) that would write 4 s after GO; the owner is killed within
        // well under a second, and the window below extends past that write.
        std::thread::sleep(Duration::from_millis(4500));
        assert!(
            !late.exists(),
            "a descendant wrote a file after the owner was killed"
        );
        // The durable record explains what happened without claiming success.
        let state: Value = wait_for("the supervisor state", Instant::now() + BUDGET, || {
            std::fs::read_to_string(task_directory.join("supervisor-state.json"))
                .ok()
                .and_then(|text| serde_json::from_str(&text).ok())
        });
        assert_eq!(state["owner_lost"], true, "{state}");
        assert_eq!(state["owner_finalized"], false, "{state}");
        assert_eq!(state["cleanup_performed_by"], "supervisor", "{state}");
        assert_eq!(state["session_published"], true, "{state}");
        assert_eq!(state["session"], json!(leader), "{state}");
        assert_eq!(state["leader_pid"], json!(leader), "{state}");
        assert_eq!(
            state["leader_start_tick"].as_u64(),
            Some(leader_start),
            "{state}"
        );
        assert_eq!(state["os_cleanup_complete"], true, "{state}");
        assert_eq!(state["remaining_pids"], json!([]), "{state}");
        assert_eq!(state["output_finalized"], false, "{state}");
        assert_eq!(state["task_record_updated"], false, "{state}");
        assert_eq!(state["status"], "failed", "{state}");
        assert_eq!(state["success"], Value::Null, "{state}");
        let cleaned: Vec<u64> = state["cleaned_pids"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(Value::as_u64)
            .collect();
        // In this controlled fixture only the supervisor's SIGKILL can end the
        // session: HUP is ignored by every member and the script cannot reach
        // its end. All three recorded members must therefore be reported as
        // signalled.
        for (label, pid) in [
            ("leader", leader),
            ("background", bg_pid),
            ("subshell", sub_pid),
        ] {
            assert!(
                cleaned.contains(&u64::from(pid)),
                "the {label} PID {pid} is missing from the cleaned set: {state}"
            );
        }
        // Truthfulness of `cleaned_pids`: every PID the supervisor claims to
        // have signalled must be one observed in the recorded session, and that
        // exact instance must be gone. `remaining_pids` is not trusted for this.
        for claimed in &cleaned {
            let Some((_, start)) = members_before_kill
                .iter()
                .find(|(pid, _)| u64::from(*pid) == *claimed)
            else {
                panic!(
                    "the supervisor claims to have signalled PID {claimed}, which was not in the recorded terminal session: {state}"
                );
            };
            assert!(
                instance_exited(*claimed as u32, *start),
                "the supervisor claims to have signalled PID {claimed} (start {start}), but that instance has not exited: {state}"
            );
        }
        assert!(
            !cleaned.contains(&u64::from(sentinel_pid)),
            "the supervisor claims to have signalled the unrelated sentinel: {state}"
        );
        assert!(
            !cleaned.contains(&u64::from(owner_pid)),
            "the supervisor claims to have signalled its own owner: {state}"
        );
        assert!(
            !cleaned.contains(&u64::from(supervisor_pid)),
            "the supervisor claims to have signalled itself: {state}"
        );
        eprintln!("supervisor state after owner SIGKILL: {state}");

        // Reopen the task from a process that is not the dead owner. The owner
        // never reached its finalizer, so the record is reconciled from the
        // supervisor's verified owner-loss facts: failed, with the reason and
        // the separate cleanup/output/record facts. The output captured before
        // the death stays readable and no input is replayed.
        let reopened = super::super::tasks::execute_for_session(
            "TaskGet",
            json!({"task_id": &task_id, "block": false}),
            &root,
            &session,
        )
        .unwrap();
        eprintln!("reopened after owner SIGKILL: {reopened}");
        assert_eq!(reopened["status"], "failed", "{reopened}");
        assert!(
            reopened["stdout"]
                .as_str()
                .unwrap_or_default()
                .contains("READY"),
            "output captured before the owner died was lost: {reopened}"
        );
        assert_eq!(reopened["owner_loss"]["owner_lost"], true, "{reopened}");
        assert_eq!(reopened["owner_loss"]["reason"], "owner_lost", "{reopened}");
        assert_eq!(reopened["owner_loss"]["success"], false, "{reopened}");
        assert_eq!(
            reopened["owner_loss"]["exit_code"],
            Value::Null,
            "{reopened}"
        );
        assert_eq!(
            reopened["owner_loss"]["os_cleanup_complete"], true,
            "{reopened}"
        );
        assert_eq!(
            reopened["owner_loss"]["output_finalized"], false,
            "{reopened}"
        );
        assert_eq!(
            reopened["owner_loss"]["session_published"], true,
            "{reopened}"
        );
        assert_eq!(
            reopened["owner_loss"]["task_record_updated"], true,
            "{reopened}"
        );
        // The reconciliation is durable and idempotent.
        let stored = super::super::tasks::read_for_session(&task_id, &root, &session).unwrap();
        assert_eq!(stored["status"], "failed", "{stored}");
        let again = super::super::tasks::execute_for_session(
            "TaskGet",
            json!({"task_id": &task_id, "block": false}),
            &root,
            &session,
        )
        .unwrap();
        assert_eq!(again["status"], "failed", "{again}");
        assert_eq!(
            again["owner_loss"], reopened["owner_loss"],
            "the confirmed owner-loss facts disappeared on a repeated query: {again}"
        );
        assert_eq!(again["subject"], reopened["subject"], "{again}");
        assert_eq!(
            again["updatedAt"], reopened["updatedAt"],
            "a repeated query rewrote the terminal task record: {again}"
        );
        assert!(
            !task_directory.join("terminal-state.json").exists(),
            "a terminal state was written although the owner was killed"
        );
        // The independent supervisor exits after recording the state; it does
        // not linger as a second owner of the terminated session.
        wait_for(
            "the supervisor to exit after recording the state",
            Instant::now() + BUDGET,
            || instance_gone(supervisor_pid, supervisor_start).then_some(()),
        );
    }

    /// Requirement P2b. The owner is killed after the gate was spawned but
    /// before the supervisor learned any session identity. The gate must exit
    /// without running the user program or any descendant, the supervisor must
    /// record that no session was published instead of guessing one, and the
    /// unrelated sentinel must survive.
    ///
    /// The owner helper is this same test binary in another process. It stops
    /// at the test-only barrier in `start`, records the exact owner and gate
    /// identities, and waits to be killed; nothing in the production path
    /// changes because the barrier is `cfg(test)` only.
    #[test]
    fn owner_sigkill_before_publication_never_runs_the_user_program() {
        if let Ok(config) = std::env::var("DCL_PTY_OWNER_DEATH_HELPER") {
            owner_death_helper(&config);
            return;
        }
        use std::os::unix::process::CommandExt as _;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_path_buf();
        let session = uuid::Uuid::new_v4().to_string();
        let info = root.join("owner-info.json");
        let barrier = root.join("prepublish.json");
        let marker = root.join("USER-RAN");
        let descendant = root.join("DESCENDANT-RAN");
        // If the gate wrongly released the program, the marker appears at once
        // and the background subshell writes its own marker shortly after.
        let script = format!(
            "printf 'USER-RAN\\n'\n\
             echo ran > '{}'\n\
             ( sleep 0.5; echo desc > '{}' ) &\n\
             sleep 300\n",
            marker.display(),
            descendant.display()
        );
        let config = json!({
            "root": root,
            "session": session,
            "script": script,
            "info": info,
        });

        struct Helper(std::process::Child);
        impl Drop for Helper {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut helper = Helper(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "tools::pty_owner::tests::owner_sigkill_before_publication_never_runs_the_user_program",
                    "--nocapture",
                ])
                .env("DCL_PTY_OWNER_DEATH_HELPER", config.to_string())
                .env(prepublication_barrier::BARRIER_ENV, &barrier)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap(),
        );

        let reached: Value = wait_for(
            "the pre-publication barrier",
            Instant::now() + BUDGET,
            || {
                std::fs::read_to_string(&barrier)
                    .ok()
                    .and_then(|text| serde_json::from_str(&text).ok())
            },
        );
        let owner_pid = reached["owner_pid"].as_u64().unwrap() as u32;
        let owner_start = reached["owner_start_tick"].as_u64().unwrap();
        let gate_pid = reached["gate_pid"].as_u64().unwrap() as u32;
        let gate_start = reached["gate_start_tick"].as_u64().unwrap();
        let task_id = reached["task_id"].as_str().unwrap().to_owned();
        assert_eq!(
            reached["session"].as_str(),
            Some(session.as_str()),
            "{reached}"
        );
        let task_directory = root.join("shell-tasks").join(&session).join(&task_id);
        assert_eq!(helper.0.id(), owner_pid);
        assert_eq!(
            process_identity(owner_pid).map(|(_, start)| start),
            Some(owner_start),
            "the recorded owner instance is not the helper process"
        );

        // The gate is a live PTY session leader, and the session was not
        // published: no `session.json` or endpoint record exists yet, so the
        // supervisor cannot know this session.
        let (gate_session, observed_gate_start) =
            process_identity(gate_pid).expect("the gate is a live PTY child");
        assert_eq!(observed_gate_start, gate_start, "{reached}");
        assert_eq!(
            gate_session, gate_pid,
            "the gate is not the terminal session leader"
        );
        assert!(!task_directory.join("session.json").exists());
        assert!(!task_directory.join("terminal.json").exists());
        // The user program cannot have run: neither marker exists pre-kill.
        assert!(!marker.exists(), "the user program ran before publication");
        assert!(
            !descendant.exists(),
            "a user-program descendant ran before publication"
        );

        let supervisor_ready: Value = wait_for(
            "the supervisor ready record",
            Instant::now() + BUDGET,
            || {
                std::fs::read_to_string(task_directory.join("supervisor.ready"))
                    .ok()
                    .and_then(|text| serde_json::from_str(&text).ok())
            },
        );
        let supervisor_pid = supervisor_ready["supervisor_pid"].as_u64().unwrap() as u32;
        let (_, supervisor_start) =
            process_identity(supervisor_pid).expect("the supervisor is a separate live process");
        assert_ne!(
            supervisor_pid, owner_pid,
            "the supervisor is not an independent process"
        );
        assert_ne!(supervisor_pid, gate_pid);

        let mut sentinel = std::process::Command::new("sleep")
            .arg("300")
            .process_group(0)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let sentinel_pid = sentinel.id();
        let (sentinel_session, sentinel_start) =
            process_identity(sentinel_pid).expect("the sentinel is live");
        assert_ne!(
            sentinel_session, gate_pid,
            "the sentinel is in the gate's session"
        );
        // Cleanup covers only the captured gate session and this test's own
        // sentinel. It never surveys unrelated processes, so a gate that fails
        // to exit is reported by the assertions below rather than masked by
        // broad cleanup.
        let mut cleanup = ExactCleanup::capture(gate_pid);
        cleanup.add(gate_pid);
        cleanup.add(sentinel_pid);
        eprintln!(
            "pre-publication fixture: owner={owner_pid}(start {owner_start}) gate={gate_pid}(start {gate_start}) gate_session={gate_session} supervisor={supervisor_pid}(start {supervisor_start}) sentinel={sentinel_pid}(start {sentinel_start}) marker={} descendant={}",
            marker.exists(),
            descendant.exists()
        );

        // Kill only the exact owner process. The gate is orphaned, nothing was
        // published, and no `Drop` runs in the owner.
        assert!(process_identity(owner_pid).is_some());
        helper.0.kill().unwrap();
        let _ = helper.0.wait();

        wait_for(
            "the exact gate instance to exit",
            Instant::now() + Duration::from_secs(10),
            || instance_gone(gate_pid, gate_start).then_some(()),
        );
        assert!(
            !session_has_live_members(gate_pid),
            "the gate's terminal session outlived the owner"
        );
        assert!(
            instance_alive(sentinel_pid, sentinel_start),
            "cleanup killed or replaced a process outside the gate session"
        );
        // Give the user program and its deferred descendant writer time to run
        // if the gate had wrongly released them. They must never appear.
        std::thread::sleep(Duration::from_millis(1500));
        assert!(
            !marker.exists(),
            "the user program ran after a pre-publication owner SIGKILL"
        );
        assert!(
            !descendant.exists(),
            "a user-program descendant ran after a pre-publication owner SIGKILL"
        );

        let state: Value = wait_for("the supervisor state", Instant::now() + BUDGET, || {
            std::fs::read_to_string(task_directory.join("supervisor-state.json"))
                .ok()
                .and_then(|text| serde_json::from_str(&text).ok())
        });
        assert_eq!(state["owner_lost"], true, "{state}");
        assert_eq!(state["session_published"], false, "{state}");
        assert_eq!(state["os_cleanup_complete"], false, "{state}");
        assert_eq!(state["output_finalized"], false, "{state}");
        assert_eq!(state["task_record_updated"], false, "{state}");
        assert_eq!(state["status"], "failed", "{state}");
        assert_eq!(state["success"], Value::Null, "{state}");
        assert!(
            state["error"]
                .as_str()
                .unwrap_or_default()
                .contains("No session identity was published"),
            "{state}"
        );
        assert!(
            state.get("cleaned_pids").is_none(),
            "the supervisor claimed session cleanup without a published session: {state}"
        );
        eprintln!("supervisor state after pre-publication owner SIGKILL: {state}");
        // The independent supervisor exits after recording the state.
        wait_for(
            "the supervisor to exit after recording the state",
            Instant::now() + BUDGET,
            || instance_gone(supervisor_pid, supervisor_start).then_some(()),
        );

        // Reopen facts without replaying input. The owner died before its
        // `TaskUpdate` to `running`, so the record would otherwise stay
        // `pending` forever; the supervisor's owner-loss record turns it into a
        // terminal `failed` with the reason and the separate facts. The user
        // program never ran, so neither marker and no terminal state exists.
        let reopened = super::super::tasks::execute_for_session(
            "TaskGet",
            json!({"task_id": &task_id, "block": false}),
            &root,
            &session,
        )
        .unwrap();
        eprintln!("reopened after pre-publication owner SIGKILL: {reopened}");
        assert_eq!(reopened["status"], "failed", "{reopened}");
        assert_eq!(reopened["owner_loss"]["owner_lost"], true, "{reopened}");
        assert_eq!(reopened["owner_loss"]["reason"], "owner_lost", "{reopened}");
        assert_eq!(reopened["owner_loss"]["success"], false, "{reopened}");
        assert_eq!(
            reopened["owner_loss"]["exit_code"],
            Value::Null,
            "{reopened}"
        );
        assert_eq!(
            reopened["owner_loss"]["os_cleanup_complete"], false,
            "{reopened}"
        );
        assert_eq!(
            reopened["owner_loss"]["output_finalized"], false,
            "{reopened}"
        );
        assert_eq!(
            reopened["owner_loss"]["session_published"], false,
            "{reopened}"
        );
        assert_eq!(
            reopened["owner_loss"]["task_record_updated"], true,
            "{reopened}"
        );
        assert!(
            !reopened["stdout"]
                .as_str()
                .unwrap_or_default()
                .contains("USER-RAN"),
            "the user program output appeared after a pre-publication owner death: {reopened}"
        );
        let stored = super::super::tasks::read_for_session(&task_id, &root, &session).unwrap();
        assert_eq!(stored["status"], "failed", "{stored}");
        let again = super::super::tasks::execute_for_session(
            "TaskGet",
            json!({"task_id": &task_id, "block": false}),
            &root,
            &session,
        )
        .unwrap();
        assert_eq!(again["status"], "failed", "{again}");
        assert_eq!(
            again["owner_loss"], reopened["owner_loss"],
            "the confirmed owner-loss facts disappeared on a repeated query: {again}"
        );
        assert_eq!(
            again["updatedAt"], reopened["updatedAt"],
            "a repeated query rewrote the terminal task record: {again}"
        );
        assert!(!task_directory.join("terminal-state.json").exists());
        assert!(!marker.exists());
        assert!(!descendant.exists());
    }
}
