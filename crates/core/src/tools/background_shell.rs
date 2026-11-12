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

/// The durable directory of a managed shell task in this session.
pub(super) fn task_directory(root: &Path, session: &str, task: &Value) -> Result<PathBuf> {
    directory(root, session, task)
}

/// The kernel-held lifetime lease of the process that owns a managed shell.
///
/// The owner holds an exclusive `flock` on `owner.lock` for as long as it runs
/// the shell task. `Drop` releases it explicitly because the lock belongs to
/// the open file description, not to one descriptor: a descriptor duplicated by
/// `try_clone` or inherited across `fork` shares that description, so closing
/// only this handle would keep the lease held until every sharer closes it.
/// The descriptor is opened `O_CLOEXEC` (Rust's default), so a shell this
/// process starts after `exec` does not inherit the lease.
///
/// Boundary: `Drop` does not run on `SIGKILL`. A child that inherited the
/// descriptor before `exec` can therefore hold the lease after the original
/// owner dies, until it closes the descriptor, execs or exits. The lease proves
/// "no descriptor still holds it" only under that documented limit; it is not a
/// hard-crash guarantee that the original process is alive.
#[derive(Debug)]
pub(crate) struct OwnerLease(Option<std::fs::File>);

#[cfg(unix)]
impl Drop for OwnerLease {
    fn drop(&mut self) {
        let Some(file) = &self.0 else {
            return;
        };
        use rustix::fs::{flock, FlockOperation};
        // An unlock does not block, so a retry only covers a signal arriving in
        // the syscall: leaving the lease held here would make a finished task
        // look live.
        loop {
            match flock(file, FlockOperation::Unlock) {
                Ok(()) => break,
                Err(rustix::io::Errno::INTR) => continue,
                Err(error) => {
                    tracing::warn!("Could not release the shell owner lease: {error}");
                    break;
                }
            }
        }
    }
}

#[cfg(not(unix))]
impl Drop for OwnerLease {
    fn drop(&mut self) {}
}

/// What the recorded owner identity proves right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OwnerState {
    /// The lease is held *and* the process recorded as the original owner is
    /// demonstrably still alive. On Linux that means a pinned process descriptor
    /// for the same boot and creation identity has not reported an exit.
    Held,
    /// The owner is gone: its lease is released, its recorded process exited, or
    /// the identity no longer matches the process with that PID.
    Gone(String),
    /// A record written before the lease existed: only a live PID is known.
    LegacyPid(u32),
    /// The identity is missing, malformed or unreadable, so it proves nothing.
    Unreadable(String),
}

/// The identity of the engine process that owns a shell task.
///
/// A bare PID is not an identity: the PID space is reused, so "a process with
/// this PID exists" can become true again after the owner dies. The boot ID
/// isolates reboots, where `/proc` start times restart from a different origin,
/// and the start time distinguishes two processes that held the same PID in one
/// boot. This is written into the lease file itself before it is published, so
/// a reader never sees a held lease without an identity that belongs to it.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, PartialEq, Eq)]
struct OwnerIdentity {
    pid: u32,
    boot: String,
    start: u64,
}

#[cfg(target_os = "linux")]
impl OwnerIdentity {
    /// The identity of this process: the engine that is about to own the shell.
    fn current() -> Result<Self> {
        let pid = std::process::id();
        Ok(Self {
            pid,
            boot: read_boot_id().context("Reading the machine boot identity")?,
            start: process_start_time(pid)
                .with_context(|| format!("Reading the creation identity of PID {pid}"))?,
        })
    }

    fn encode(&self) -> String {
        format!("dcl-owner-v1 {} {} {}\n", self.pid, self.boot, self.start)
    }

    /// Parses a recorded identity. Anything that is not exactly one complete,
    /// well-formed record yields `None`: a partial or foreign line must never be
    /// read as "some process owns this".
    fn parse(text: &str) -> Option<Self> {
        // Exactly one record: trailing non-empty content means this file is not
        // the record this build wrote, so it must not be read as an identity.
        let mut lines = text.lines();
        let first = lines.next()?;
        if lines.any(|line| !line.trim().is_empty()) {
            return None;
        }
        let mut fields = first.split_whitespace();
        if fields.next()? != "dcl-owner-v1" {
            return None;
        }
        let pid = fields.next()?.parse::<u32>().ok()?;
        let boot = fields.next()?.to_owned();
        let start = fields.next()?.parse::<u64>().ok()?;
        if fields.next().is_some() || boot.is_empty() {
            return None;
        }
        // PID 0 is the kernel's idle task and can never own a shell; anything
        // above the kernel's `PID_MAX_LIMIT` is not a PID this system can issue.
        if !valid_pid(pid) {
            return None;
        }
        Some(Self { pid, boot, start })
    }

    /// Whether the process with this PID is, right now, the very process this
    /// identity was recorded for.
    ///
    /// `Ok(false)` is a definite mismatch (the process is gone, was reaped, or
    /// the PID now names a different process). `Err` means the identity could
    /// not be checked, which never authorizes a write.
    fn matches_current_process(&self) -> std::io::Result<bool> {
        if read_boot_id()? != self.boot {
            return Ok(false);
        }
        match process_start_time(self.pid) {
            Ok(start) => Ok(start == self.start),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
}

/// Linux `PID_MAX_LIMIT`: the largest PID the kernel can ever allocate.
#[cfg(target_os = "linux")]
const PID_MAX_LIMIT: u32 = 4 * 1024 * 1024;

/// A PID that can name a real process: never 0 (the idle task), never above the
/// kernel's allocatable range.
#[cfg(target_os = "linux")]
fn valid_pid(pid: u32) -> bool {
    pid > 0 && pid <= PID_MAX_LIMIT
}

#[cfg(target_os = "linux")]
fn read_boot_id() -> std::io::Result<String> {
    Ok(std::fs::read_to_string("/proc/sys/kernel/random/boot_id")?
        .trim()
        .to_owned())
}

/// Field 22 of `/proc/<pid>/stat`: the process creation time in clock ticks.
///
/// The command name is parenthesized and may itself contain spaces or
/// parentheses, so the fixed fields are taken after the last `)`.
#[cfg(target_os = "linux")]
fn process_start_time(pid: u32) -> std::io::Result<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    stat.rsplit_once(')')
        .and_then(|(_, rest)| rest.split_whitespace().nth(19))
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("the creation field of PID {pid} is unreadable"),
            )
        })
}

/// Whether a pinned process descriptor reports that its process has exited. A
/// zombie counts as exited: nothing can consume stdin any more.
#[cfg(target_os = "linux")]
fn pidfd_exited(pidfd: &rustix::fd::OwnedFd) -> std::io::Result<bool> {
    let mut fds = [rustix::event::PollFd::new(
        pidfd,
        rustix::event::PollFlags::IN,
    )];
    let timeout = rustix::event::Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    loop {
        match rustix::event::poll(&mut fds, Some(&timeout)) {
            Ok(0) => return Ok(false),
            Ok(_) => return Ok(true),
            Err(rustix::io::Errno::INTR) => continue,
            Err(error) => return Err(error.into()),
        }
    }
}

impl OwnerState {
    pub(crate) fn is_held(&self) -> bool {
        matches!(self, Self::Held)
    }

    /// Why a write must not use this channel.
    pub(crate) fn refusal(&self, task: &str) -> String {
        match self {
            Self::Held => String::new(),
            Self::Gone(detail) => format!(
                "Task {task} stdin is unavailable: {detail}.                  TaskStop records cancellation, but only the owner can terminate its shell"
            ),
            Self::LegacyPid(pid) => format!(
                "Task {task} stdin is unavailable: it records only a PID ({pid}) and no owner lease,                  so a live PID cannot prove this stdin channel still has its real owner. Start a new interactive shell"
            ),
            Self::Unreadable(detail) => format!(
                "Task {task} stdin is unavailable: {detail}, so the owning process cannot be established"
            ),
        }
    }
}

/// A cached probe for one task's owner lease.
///
/// Probing takes a *shared* lock so two readers never mistake each other for the
/// owner; an owner's exclusive lease makes that probe fail with `EWOULDBLOCK`.
pub(crate) struct OwnerProbe {
    directory: PathBuf,
    lease: Option<std::fs::File>,
    /// The identity recorded inside the lease file, when there is one.
    #[cfg(target_os = "linux")]
    identity: Option<OwnerIdentity>,
    /// The process descriptor pinned for that identity, reused across probes so
    /// a PID reused after the owner's death still cannot satisfy the check.
    #[cfg(target_os = "linux")]
    pidfd: std::sync::Mutex<Option<(OwnerIdentity, rustix::fd::OwnedFd)>>,
}

impl OwnerProbe {
    pub(crate) fn open(directory: &Path) -> Result<Self> {
        let lease = match std::fs::File::open(directory.join("owner.lock")) {
            Ok(file) => Some(file),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Opening the owner lease in {}", directory.display()))
            }
        };
        // The identity travels inside the lease file itself, so it is published
        // atomically with the lock and can never be missing from a lease this
        // build wrote.
        #[cfg(target_os = "linux")]
        let identity = lease.as_ref().and_then(|file| {
            let mut text = String::new();
            use std::io::Read;
            (&mut &*file).read_to_string(&mut text).ok()?;
            OwnerIdentity::parse(&text)
        });
        Ok(Self {
            directory: directory.to_owned(),
            lease,
            #[cfg(target_os = "linux")]
            identity,
            #[cfg(target_os = "linux")]
            pidfd: std::sync::Mutex::new(None),
        })
    }

    /// Re-probe the owner. Called when a write starts and again on every wait.
    pub(crate) fn state(&self) -> OwnerState {
        if let Some(lease) = &self.lease {
            return match lease_held(lease) {
                // A held lease only proves that some descriptor still holds the
                // open file description, which an inherited or duplicated
                // descriptor does even after the real owner was killed. Linux
                // additionally requires the recorded original process to be
                // demonstrably alive.
                Ok(true) => self.held_owner_state(),
                Ok(false) => OwnerState::Gone(
                    "the owner lease is released, so the shell's owner has stopped running it"
                        .into(),
                ),
                Err(error) => {
                    OwnerState::Unreadable(format!("the owner lease cannot be probed: {error}"))
                }
            };
        }
        // Records written before the lease existed carry only a PID; a reused
        // PID cannot be distinguished there, which is why it is not accepted for
        // interactive writes.
        match std::fs::read_to_string(self.directory.join("owner.pid")) {
            Ok(text) => match text.trim().parse::<u32>() {
                Ok(pid) if pid > 0 => {
                    if crate::services::session_worker::process_exited(pid) {
                        OwnerState::Gone(format!(
                            "the recorded owner process (PID {pid}) has exited"
                        ))
                    } else {
                        OwnerState::LegacyPid(pid)
                    }
                }
                _ => OwnerState::Unreadable("the recorded owner PID is malformed".into()),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                OwnerState::Unreadable("this task records no owner lease or owner PID".into())
            }
            Err(error) => {
                OwnerState::Unreadable(format!("the recorded owner PID cannot be read: {error}"))
            }
        }
    }

    /// A held lease is necessary but not sufficient on Linux.
    ///
    /// The recorded identity is checked against `/proc` and then pinned with a
    /// `pidfd`, which the kernel keeps pointing at that same process even if its
    /// PID is reused. Every probe polls the pinned descriptor again, so a death
    /// after the first check is still seen. On other platforms the lease remains
    /// the only signal; that is *not* proof of a live owner and is documented as
    /// unverified.
    #[cfg(not(target_os = "linux"))]
    fn held_owner_state(&self) -> OwnerState {
        // The lease proves only that some descriptor holds the open file
        // description. Without the Linux process-identity check there is no
        // equivalent proof that the recorded owner is alive, so interactive
        // input is refused here rather than reported as `Held`. Ordinary Bash
        // output and TaskGet keep working: this path only guards stdin.
        OwnerState::Unreadable(
            "this platform cannot prove that the recorded shell owner is still alive (a Linux process identity is required), so interactive stdin is not supported here yet; Bash output and TaskGet still work".into(),
        )
    }

    #[cfg(target_os = "linux")]
    fn held_owner_state(&self) -> OwnerState {
        let Some(identity) = &self.identity else {
            // An empty or pre-identity lease is readable for TaskGet (the logs
            // are not affected), but it cannot authorize interactive input:
            // manufacturing an identity from whichever process holds the lock
            // now would re-create the very confusion this check removes.
            return OwnerState::Unreadable(
                "the owner lease records no process identity, so a live owner cannot be established on this platform; start a new interactive shell".into(),
            );
        };
        let mut pinned = self.pidfd.lock().unwrap_or_else(|error| error.into_inner());
        // Reuse the descriptor only for this same recorded identity: it already
        // names the very process that was validated below.
        let reuse = pinned
            .as_ref()
            .is_some_and(|(identity, _)| identity == self.identity.as_ref().unwrap());
        if !reuse {
            match self.pin_owner(identity) {
                Ok(fd) => *pinned = Some((identity.clone(), fd)),
                Err(state) => return state,
            }
        }
        let (_, pidfd) = pinned.as_ref().expect("a pinned owner was just stored");
        match pidfd_exited(pidfd) {
            Ok(true) => OwnerState::Gone(format!(
                "the recorded owner process (PID {}) has exited",
                identity.pid
            )),
            Ok(false) => OwnerState::Held,
            Err(error) => OwnerState::Unreadable(format!(
                "the recorded owner process (PID {}) cannot be observed: {error}",
                identity.pid
            )),
        }
    }

    /// Opens a process descriptor for the recorded owner, checking the creation
    /// identity both before and after the open so a PID reused in that window
    /// cannot be mistaken for the original process.
    #[cfg(target_os = "linux")]
    fn pin_owner(
        &self,
        identity: &OwnerIdentity,
    ) -> std::result::Result<rustix::fd::OwnedFd, OwnerState> {
        let mismatch = || {
            OwnerState::Gone(format!(
                "the recorded owner identity (PID {} from boot {}) does not match the process with that PID now, so the original owner is gone",
                identity.pid, identity.boot
            ))
        };
        match identity.matches_current_process() {
            Ok(true) => {}
            Ok(false) => return Err(mismatch()),
            Err(error) => {
                return Err(OwnerState::Unreadable(format!(
                    "the recorded owner identity (PID {}) cannot be checked: {error}",
                    identity.pid
                )))
            }
        }
        let pid = rustix::process::Pid::from_raw(identity.pid as i32)
            .ok_or_else(|| OwnerState::Unreadable("the recorded owner PID is not usable".into()))?;
        let pidfd = match rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()) {
            Ok(pidfd) => pidfd,
            Err(rustix::io::Errno::SRCH) => return Err(mismatch()),
            Err(error) => {
                let detail = format!(
                    "a process descriptor for the recorded owner (PID {}) cannot be opened: {error}",
                    identity.pid
                );
                return Err(OwnerState::Unreadable(detail));
            }
        };
        // The identity must still describe the process the descriptor pins; a
        // death and PID reuse between the reads above fails here.
        match identity.matches_current_process() {
            Ok(true) => Ok(pidfd),
            Ok(false) => Err(mismatch()),
            Err(error) => Err(OwnerState::Unreadable(format!(
                "the recorded owner identity (PID {}) cannot be confirmed: {error}",
                identity.pid
            ))),
        }
    }
}

/// Take the exclusive lease for a shell this process is about to run.
///
/// The lock is taken on a private file that is published with a hard link only
/// once it is locked, so `owner.lock` never exists in an unlocked state.
/// Otherwise a concurrent TaskGet/TaskInput probe could take its shared probe
/// lock in the gap between creating the file and locking it, making this
/// acquisition fail with a false conflict and leaving a published task record
/// behind.
///
/// On Linux the file also carries this process's identity (PID, boot ID and
/// creation time). It is written before publication, so a held lease always
/// names its owner; a separate file written afterwards could be missing or stale
/// for a moment and that window is exactly when a probe runs.
#[cfg(unix)]
fn take_lease(directory: &Path, id: &str) -> Result<OwnerLease> {
    use rustix::fs::{flock, FlockOperation};
    let staging = directory.join(format!("owner.lock.new-{}", uuid::Uuid::new_v4()));
    let lease = directory.join("owner.lock");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&staging)
        .with_context(|| format!("Creating the owner lease for task {id}"))?;
    let result = record_owner_identity(&file, id)
        .and_then(|()| {
            flock(&file, FlockOperation::NonBlockingLockExclusive).map_err(std::io::Error::from)
        })
        // Linking, unlike renaming, never replaces an existing lease record, so
        // a second owner cannot substitute the identity of a live one.
        .and_then(|()| std::fs::hard_link(&staging, &lease));
    let _ = std::fs::remove_file(&staging);
    match result {
        Ok(()) => Ok(OwnerLease(Some(file))),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            // The record is never replaced, but its existence alone proves
            // nothing: an owner releases this lock by exiting, and the file
            // stays behind as the identity a new task must not reuse.
            anyhow::bail!(
                "Task {id} already records an owner lock at {}; an existing owner record is never replaced. It may be free if its owner exited, so its existence alone does not prove a live owner",
                lease.display()
            )
        }
        Err(error) => Err(error).with_context(|| format!("Taking the owner lease for task {id}")),
    }
}

/// Writes the owning process's identity into the lease before it is published.
#[cfg(unix)]
fn record_owner_identity(file: &std::fs::File, id: &str) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::io::Write;
        let identity = OwnerIdentity::current().map_err(std::io::Error::other)?;
        let text = identity.encode();
        (&*file).write_all(text.as_bytes()).map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!("Recording the owner identity for task {id}: {error}"),
            )
        })?;
        (&*file).sync_all()
    }
    // Other platforms record no identity: their lease is a lock only, which is
    // documented as unverified for liveness.
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (file, id);
        Ok(())
    }
}

/// Windows and other non-POSIX targets have no advisory-lock lease; the PID
/// record stays the only signal there, and interactive pipe mode is unavailable.
#[cfg(not(unix))]
fn take_lease(_directory: &Path, _id: &str) -> Result<OwnerLease> {
    Ok(OwnerLease(None))
}

#[cfg(unix)]
fn lease_held(file: &std::fs::File) -> std::io::Result<bool> {
    use rustix::fs::{flock, FlockOperation};
    match flock(file, FlockOperation::NonBlockingLockShared) {
        Ok(()) => {
            // Free: drop the probe lock so it cannot block the next owner.
            let _ = flock(file, FlockOperation::Unlock);
            Ok(false)
        }
        Err(rustix::io::Errno::WOULDBLOCK) => Ok(true),
        Err(error) => Err(error.into()),
    }
}

#[cfg(not(unix))]
fn lease_held(_file: &std::fs::File) -> std::io::Result<bool> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "POSIX file leases are unavailable on this platform",
    ))
}

/// Shared status presentation without loading process logs.
pub fn task_status(root: &Path, session: &str, task: &Value) -> Result<String> {
    let directory = directory(root, session, task)?;
    let fresh = std::fs::metadata(directory.join("heartbeat"))
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|time| time.elapsed().ok())
        .is_some_and(|age| age < Duration::from_secs(10));
    // A fresh heartbeat outlives an abruptly killed owner by seconds; a lease
    // that is released, or whose recorded original process is gone, proves that
    // nothing can consume the task's input any more. Unknown identities keep the
    // historical heartbeat rule so old records stay readable.
    let owner_gone = OwnerProbe::open(&directory)
        .is_ok_and(|probe| matches!(probe.state(), OwnerState::Gone(_)));
    Ok(if task["status"] == "running" && (owner_gone || !fresh) {
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

/// How a managed shell receives stdin. `StdinPipe` is an explicit opt-in and is
/// not a terminal: it gives the model a writable pipe, not a PTY.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Interaction {
    None,
    StdinPipe,
}

impl Interaction {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::StdinPipe => "pipe",
        }
    }
}

/// The resources a managed shell owns once its task record exists.
#[derive(Debug)]
struct StartedShell {
    lease: OwnerLease,
    stdin_pipe: Option<std::fs::File>,
    keepalive: Option<std::fs::File>,
}

/// Record a startup failure on the task `TaskCreate` already made.
///
/// The original error is always surfaced. If the terminal update also fails,
/// the caller receives both diagnostics and no claim that a failed record was
/// durably stored: the record may still show its previous status.
fn fail_startup(root: &Path, session: &str, id: &str, error: anyhow::Error) -> anyhow::Error {
    let output = json!({"is_error":true,"startup_failed":true,"error":format!("{error:#}")});
    match super::tasks::execute_for_shell_owner(
        "TaskUpdate",
        json!({"id":id,"status":"failed","output":output.to_string()}),
        root,
        session,
    ) {
        Ok(_) => error,
        Err(record_error) => anyhow::anyhow!(
            "Shell task startup failed: {error:#}. Recording the failure also failed: {record_error:#}; the task record may still show its previous status."
        ),
    }
}

/// Finish starting a task whose record `TaskCreate` already created.
///
/// Every failure after the record exists goes through `fail_startup`, so an
/// initialization error never leaves a pending or falsely running task. A
/// conflicting `owner.lock` is never replaced: `take_lease` fails instead.
fn initialize_start(
    root: &Path,
    session: &str,
    id: &str,
    directory: &Path,
    interaction: Interaction,
) -> Result<StartedShell> {
    let initialized = (|| -> Result<StartedShell> {
        std::fs::create_dir_all(directory)
            .with_context(|| format!("Creating the task directory for task {id}"))?;
        // The process that owns this task (its shell and its stdin pipe) holds
        // a lifetime lease; the PID is kept only for diagnostics and old
        // readers.
        let lease = take_lease(directory, id)?;
        std::fs::write(directory.join("owner.pid"), std::process::id().to_string())
            .with_context(|| format!("Recording the owner PID for task {id}"))?;
        let (stdin_pipe, keepalive) = match interaction {
            Interaction::None => (None, None),
            Interaction::StdinPipe => {
                let (child_stdin, owner_writer) = prepare_stdin_pipe(directory, id)?;
                (Some(child_stdin), Some(owner_writer))
            }
        };
        super::tasks::execute_for_shell_owner(
            "TaskUpdate",
            json!({"id":id,"status":"running"}),
            root,
            session,
        )?;
        std::fs::write(directory.join("heartbeat"), b"running")
            .with_context(|| format!("Writing the heartbeat for task {id}"))?;
        Ok(StartedShell {
            lease,
            stdin_pipe,
            keepalive,
        })
    })();
    initialized.map_err(|error| fail_startup(root, session, id, error))
}

/// Publish ownership for a foreground call that was moved to the background.
fn initialize_promotion(
    root: &Path,
    session: &str,
    id: &str,
    session_directory: &Path,
    directory: &Path,
) -> Result<OwnerLease> {
    let initialized = (|| -> Result<OwnerLease> {
        // The task ID directory marks a managed shell for blocking TaskGet
        // waits; the logs stay where the call started.
        std::fs::create_dir_all(session_directory.join(id))
            .with_context(|| format!("Creating the promoted task directory for task {id}"))?;
        let lease = take_lease(directory, id)?;
        std::fs::write(directory.join("owner.pid"), std::process::id().to_string())
            .with_context(|| format!("Recording the owner PID for task {id}"))?;
        super::tasks::execute_for_shell_owner(
            "TaskUpdate",
            json!({"id":id,"status":"running"}),
            root,
            session,
        )?;
        std::fs::write(directory.join("heartbeat"), b"running")
            .with_context(|| format!("Writing the heartbeat for task {id}"))?;
        Ok(lease)
    })();
    initialized.map_err(|error| fail_startup(root, session, id, error))
}

pub(crate) fn start(
    mut command: Command,
    timeout: Option<Duration>,
    shell: &str,
    description: &str,
    command_text: &str,
    guard: Option<crate::services::wsl::Guard>,
    interaction: Interaction,
) -> Result<Value> {
    let (root, session) = CONTEXT
        .try_with(Clone::clone)
        .ok()
        .flatten()
        .context("Background Bash requires an owning engine session")?;
    // A mode this platform cannot honour must be refused before a task record
    // exists, so a failed start never leaves a pending managed shell behind.
    if interaction == Interaction::StdinPipe && !cfg!(unix) {
        anyhow::bail!("Bash interaction:\"pipe\" needs a POSIX stdin pipe; interactive stdin is not implemented on this platform yet (Windows named pipes arrive with the PTY increment)");
    }
    let mut metadata = json!({"managed_shell":true,"command":command_text,"started_ms":chrono::Utc::now().timestamp_millis()});
    if interaction == Interaction::StdinPipe {
        metadata["interaction"] = json!("pipe");
    }
    let task = super::tasks::execute_for_shell_owner(
        "TaskCreate",
        // The command and start time are kept for the task details view.
        json!({"subject":description,"metadata":metadata}),
        &root,
        &session,
    )?;
    let id = task["id"].as_str().context("Task ID missing")?.to_string();
    let directory = root.join("shell-tasks").join(&session).join(&id);
    let StartedShell {
        lease,
        stdin_pipe,
        keepalive,
    } = initialize_start(&root, &session, &id, &directory, interaction)?;
    let stdout = directory.join("stdout.log");
    let stderr = directory.join("stderr.log");
    let shell = shell.to_string();
    let logs = (stdout.clone(), stderr.clone());
    let task_directory = directory.clone();
    ACTIVE.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    let mut completion = Completion {
        root,
        session,
        id: id.clone(),
        done: false,
    };
    tokio::spawn(async move {
        let _guard = guard;
        // Held until this owner task ends: the kernel releases the lease when
        // the process exits, so no PID reuse can impersonate it.
        let _lease = lease;
        // The owner keeps one write side of the FIFO open. Dropping it on the
        // EOF marker is what the shell observes as end of input; transient
        // writers can then no longer keep the pipe open by accident.
        let mut keepalive = keepalive;
        let eof_marker = task_directory.join("stdin-eof");
        let cancel_marker = task_directory.join("cancel");
        let heartbeat_path = task_directory.join("heartbeat");
        let supervise = async {
            let mut heartbeat = std::time::Instant::now();
            loop {
                if cancel_marker.exists() {
                    break;
                }
                if keepalive.is_some() && eof_marker.exists() {
                    drop(keepalive.take());
                }
                if heartbeat.elapsed() >= Duration::from_secs(1) {
                    let _ = std::fs::write(&heartbeat_path, b"running");
                    heartbeat = std::time::Instant::now();
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };
        let result = tokio::select! {
            result = super::bash_process::run_logged(&mut command, timeout, &shell, Some(logs), stdin_pipe) => result,
            _ = supervise => {
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
    let mut result = json!({"task_id":id,"subject":description,"status":"running","stdout_file":stdout,"stderr_file":stderr,"message":format!("Background command \"{description}\" started. You will be notified when it finishes. To read its output earlier, load TaskGet with ToolSearch; TaskStop stops it.")});
    if interaction == Interaction::StdinPipe {
        result["interaction"] = json!("pipe");
        result["stdin_available"] = json!(true);
        result["stdin_pipe"] = json!(directory.join("stdin.pipe"));
        result["message"] = json!(format!("Background command \"{description}\" is running with a writable stdin pipe. Use TaskInput with task_id to write UTF-8 data (eof:true closes stdin), TaskGet with ToolSearch to read output, and TaskStop to stop it. You will be notified when it finishes."));
    }
    Ok(result)
}

/// Create the task's stdin FIFO and open both ends the shell and the owner need.
///
/// The write+read handle is opened first so the read-only open cannot block and
/// so the shell never sees EOF while the owner is alive. The shell's handle is
/// opened read-only: a shared read+write handle would itself act as a writer and
/// hide EOF from the shell forever.
#[cfg(unix)]
fn prepare_stdin_pipe(directory: &Path, id: &str) -> Result<(std::fs::File, std::fs::File)> {
    use rustix::fs::{mkfifoat, open, statat, AtFlags, Mode, OFlags};
    let path = directory.join("stdin.pipe");
    match statat(rustix::fs::CWD, &path, AtFlags::empty()) {
        Ok(_) => std::fs::remove_file(&path).context("Replacing a stale stdin pipe")?,
        Err(rustix::io::Errno::NOENT) => {}
        Err(error) => return Err(error).context("Inspecting the task stdin pipe"),
    }
    mkfifoat(rustix::fs::CWD, &path, Mode::from_raw_mode(0o600))
        .context("Creating the task stdin pipe")?;
    let owner = open(&path, OFlags::RDWR | OFlags::CLOEXEC, Mode::empty())
        .map_err(|error| anyhow::anyhow!("Opening the owner side of task {id} stdin: {error}"))?;
    let child = open(&path, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
        .map_err(|error| anyhow::anyhow!("Opening the shell side of task {id} stdin: {error}"))?;
    Ok((std::fs::File::from(child), std::fs::File::from(owner)))
}

#[cfg(not(unix))]
fn prepare_stdin_pipe(_directory: &Path, _id: &str) -> Result<(std::fs::File, std::fs::File)> {
    anyhow::bail!("Bash interaction:\"pipe\" needs a POSIX stdin pipe; interactive stdin is not implemented on this platform yet (Windows named pipes arrive with the PTY increment)")
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
        super::bash_process::run_logged(&mut command, Some(timeout), &shell_name, Some(logs), None)
            .await
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
    // The task ID directory stays as the marker that makes blocking TaskGet wait
    // for a managed shell; ownership metadata goes where the common directory
    // resolver looks, which for a promoted call is its `fg-` log directory.
    let lease = match initialize_promotion(&root, &session, &id, &session_directory, &directory) {
        Ok(lease) => lease,
        Err(error) => {
            // The foreground call is already running and cannot be promoted:
            // stop the exact process tree it owns instead of leaving it behind
            // a failed record.
            if let Some(handle) = running.0.take() {
                handle.abort();
                let _ = handle.await;
            }
            return Err(error);
        }
    };
    ACTIVE.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    let mut completion = Completion {
        root,
        session,
        id: id.clone(),
        done: false,
    };
    let mut handle = running.0.take().context("Shell task missing")?;
    tokio::spawn(async move {
        let _lease = lease;
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
    // Bounded page mode returns only the requested window plus a cursor for the
    // next one. Ownership was already enforced by `read_for_session` above.
    let page = crate::tools::output_page::PageRequest::parse(input)?;
    if matches!(&page, crate::tools::output_page::PageRequest::Page { .. }) {
        return Ok(Some(page_task_get(task, &id, status, &directory, &page)?));
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

/// Explicit indicator for bytes that could not be decoded, so a caller never
/// mistakes replaced bytes for real output.
fn invalid_gap(stdout: usize, stderr: usize) -> Value {
    if stdout == 0 && stderr == 0 {
        Value::Null
    } else {
        json!({"reason":"invalid_utf8","stdout_bytes":stdout,"stderr_bytes":stderr})
    }
}

/// One bounded page of a managed shell's append-only logs. Each stream is read
/// with a seek, so a multi-gigabyte log is never loaded before trimming.
fn page_task_get(
    task: Value,
    id: &str,
    status: Value,
    directory: &Path,
    request: &crate::tools::output_page::PageRequest,
) -> Result<Value> {
    use crate::tools::output_page::{self, Cursor, PageRequest};
    let PageRequest::Page { cursor, max_bytes } = request else {
        unreachable!("page_task_get is only called for page requests")
    };
    let stdout_path = directory.join("stdout.log");
    let stderr_path = directory.join("stderr.log");
    let done = terminal(&status);
    let (out, err) = cursor
        .as_ref()
        .map(|cursor| (cursor.out(), cursor.err()))
        .unwrap_or((0, 0));
    let mut stdout_stream = output_page::open_stream(&stdout_path, out)?;
    let mut stderr_stream = output_page::open_stream(&stderr_path, err)?;
    if let Some(cursor) = cursor {
        cursor.validate(id, "shell")?;
        // Validate against the same opened snapshots that will be read, so a
        // rotation between checking and reading cannot mix identities.
        cursor.validate_stream_at(
            out,
            0,
            cursor.out_gen(),
            &output_page::stream_generation_of(&stdout_stream),
        )?;
        cursor.validate_stream_at(
            err,
            0,
            cursor.err_gen(),
            &output_page::stream_generation_of(&stderr_stream),
        )?;
    }
    let stdout = output_page::read_stream_page(stdout_stream.as_mut(), out, *max_bytes, done)?;
    // The bound covers all returned log text, not each stream separately.
    let remaining = max_bytes.saturating_sub(stdout.text.len() as u64);
    let stderr = output_page::read_stream_page(stderr_stream.as_mut(), err, remaining, done)?;
    let next = Cursor::new(
        id,
        "shell",
        stdout.generation.clone(),
        stderr.generation.clone(),
        stdout.next,
        stderr.next,
    );
    let truncated = stdout.truncated || stderr.truncated;
    let mut value = task;
    value["task_id"] = json!(id);
    value["status"] = status.clone();
    value["stdout"] = json!(stdout.text);
    value["stderr"] = json!(stderr.text);
    // Page mode exposes only the page; the stored text is never repeated.
    value["output"] = Value::Null;
    value["result"] = Value::Null;
    value["block"] = json!(false);
    value["retrieval_status"] = json!(if done { "success" } else { "not_ready" });
    value["output_gap"] = invalid_gap(stdout.invalid_bytes, stderr.invalid_bytes);
    value["output_mode"] = json!("page");
    value["next_cursor"] = json!(next.encode());
    value["has_more"] = json!(truncated);
    value["output_truncated"] = json!(truncated);
    value["max_output_bytes"] = json!(max_bytes);
    Ok(value)
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
    let page = crate::tools::output_page::PageRequest::parse(input)?;
    let mut last = json!({"task_id":id,"status":"unknown","stdout":"","stderr":"","result":null,"block":block,"retrieval_status":"timeout"});
    if let crate::tools::output_page::PageRequest::Page { max_bytes, .. } = page {
        // A timeout must still return a bounded page, never a promise of output.
        last["output_mode"] = json!("page");
        last["next_cursor"] = Value::Null;
        last["has_more"] = json!(false);
        last["output_truncated"] = json!(false);
        last["max_output_bytes"] = json!(max_bytes);
    }
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
                super::Interaction::None,
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
            start(
                command,
                None,
                "node",
                "live fixture",
                "node fixture",
                None,
                super::Interaction::None,
            )
            .unwrap()
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

    /// A failure bound for observing producer readiness. It is not a pass
    /// condition: the log content itself must appear before the wait ends.
    const READINESS_DEADLINE: Duration = Duration::from_secs(30);

    fn read_pid(path: &Path) -> Option<u32> {
        std::fs::read_to_string(path).ok()?.trim().parse().ok()
    }

    /// Test-owned child processes. If the test fails before it cancels them,
    /// exactly these PIDs are killed: no pattern matching and no negative PID.
    struct OwnedProcesses {
        markers: Vec<PathBuf>,
        armed: bool,
    }
    impl OwnedProcesses {
        fn new(markers: Vec<PathBuf>) -> Self {
            Self {
                markers,
                armed: true,
            }
        }
        fn disarm(&mut self) {
            self.armed = false;
        }
    }
    impl Drop for OwnedProcesses {
        fn drop(&mut self) {
            if !self.armed {
                return;
            }
            for marker in &self.markers {
                if let Some(pid) = read_pid(marker) {
                    kill_exact(pid);
                }
            }
        }
    }
    #[cfg(unix)]
    fn kill_exact(pid: u32) {
        unsafe extern "C" {
            fn kill(pid: i32, signal: i32) -> i32;
        }
        if pid > 0 && pid <= i32::MAX as u32 {
            unsafe {
                kill(pid as i32, 9);
            }
        }
    }
    #[cfg(windows)]
    fn kill_exact(pid: u32) {
        let _ = std::process::Command::new("taskkill.exe")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .output();
    }
    #[cfg(not(any(unix, windows)))]
    fn kill_exact(_: u32) {}

    /// True once the process can no longer run. A zombie still answers
    /// `kill(pid, 0)` but cannot produce a side effect any more.
    fn finished_process(pid: u32) -> bool {
        if crate::services::session_worker::process_exited(pid) {
            return true;
        }
        #[cfg(target_os = "linux")]
        {
            if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                return stat
                    .rsplit(')')
                    .next()
                    .and_then(|rest| rest.split_whitespace().next())
                    == Some("Z");
            }
        }
        false
    }

    fn snapshot(root: &Path, session: &str, id: &str) -> Value {
        super::super::tasks::execute_for_session(
            "TaskGet",
            json!({"task_id":id,"block":false}),
            root,
            session,
        )
        .expect("TaskGet snapshot")
    }

    fn mentions(value: &Value, stream: &str, text: &str) -> bool {
        value[stream]
            .as_str()
            .is_some_and(|content| content.contains(text))
    }

    /// Polls the non-blocking snapshot until `ready` holds. `start` only
    /// schedules the owner future, and stdout/stderr are captured independently,
    /// so a test must observe readiness rather than assume output already exists.
    async fn wait_until(
        root: &Path,
        session: &str,
        id: &str,
        what: &str,
        ready: impl Fn(&Value) -> bool,
    ) -> Value {
        let started = tokio::time::Instant::now();
        loop {
            let value = snapshot(root, session, id);
            if ready(&value) {
                return value;
            }
            assert!(
                started.elapsed() < READINESS_DEADLINE,
                "no {what} within {READINESS_DEADLINE:?}; last snapshot: {value}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn wait_for_file(path: &Path, what: &str) {
        let started = tokio::time::Instant::now();
        while !path.exists() {
            assert!(
                started.elapsed() < READINESS_DEADLINE,
                "no {what} within {READINESS_DEADLINE:?} ({})",
                path.display()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Waits until a log the owner is capturing already contains `text`.
    async fn wait_for_log_contains(path: &Path, text: &str, what: &str) {
        let started = tokio::time::Instant::now();
        loop {
            let content = std::fs::read_to_string(path).unwrap_or_default();
            if content.contains(text) {
                return;
            }
            assert!(
                started.elapsed() < READINESS_DEADLINE,
                "no {what} within {READINESS_DEADLINE:?} ({}: {content:?})",
                path.display()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn wait_process_finished(pid: u32, what: &str, diagnostics: &impl std::fmt::Display) {
        let started = tokio::time::Instant::now();
        while !finished_process(pid) {
            assert!(
                started.elapsed() < READINESS_DEADLINE,
                "{what} {pid} was still running after {READINESS_DEADLINE:?}; {diagnostics}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Starts a producer that writes nothing until the test opens a gate, then
    /// writes `ready` to stdout, waits for the second gate, and writes
    /// `diagnostic` to stderr before exiting.
    const GATED_LOG_FIXTURE: &str = r#"
const fs = require('fs');
const [pidFile, startGate, stderrGate] = process.argv.slice(2);
const waitFor = (path) => new Promise((done) => {
  const timer = setInterval(() => {
    if (fs.existsSync(path)) { clearInterval(timer); done(); }
  }, 10);
});
fs.writeFileSync(pidFile, String(process.pid));
(async () => {
  await waitFor(startGate);
  console.log('ready');
  await waitFor(stderrGate);
  console.error('diagnostic');
})();
"#;

    /// Starts the managed shell, which spawns a descendant that records its own
    /// PID. The descendant acknowledges the arm gate and only writes its leaked
    /// side effect after the leak gate is opened.
    const MANAGED_SHELL_FIXTURE: &str = r#"
const fs = require('fs');
const { spawn } = require('child_process');
const [ownPid, descendantPid, armGate, leakGate, armed, leaked] = process.argv.slice(2);
const descendant = `
const fs = require('fs');
fs.writeFileSync(process.argv[1], String(process.pid));
const [armGate, leakGate, armed, leaked] = process.argv.slice(2);
const waitFor = (path) => new Promise((done) => {
  const timer = setInterval(() => {
    if (fs.existsSync(path)) { clearInterval(timer); done(); }
  }, 10);
});
(async () => {
  await waitFor(armGate);
  fs.writeFileSync(armed, 'armed');
  await waitFor(leakGate);
  fs.writeFileSync(leaked, 'leaked');
})();
setInterval(() => {}, 1000);
`;
spawn(process.execPath, ['-e', descendant, descendantPid, armGate, leakGate, armed, leaked], { stdio: 'inherit' });
fs.writeFileSync(ownPid, String(process.pid));
console.log('ready');
console.error('diagnostic');
setInterval(() => {}, 10000);
"#;
    #[tokio::test]
    async fn a_running_foreground_call_moves_to_the_background_on_request() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let script = "console.log('early');setTimeout(()=>console.log('late'),2500)";
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
        // A promoted call keeps its task-ID marker directory for waiting, but
        // ownership metadata must live where the common directory resolver
        // looks, which for it is the `fg-` log directory.
        let running = wait_until(root.path(), &session, id, "promoted output", |value| {
            value["status"] == "running" && mentions(value, "stdout", "early")
        })
        .await;
        assert_eq!(running["status"], "running", "{running}");
        let record = super::super::tasks::read_for_session(id, root.path(), &session).unwrap();
        let resolved = super::directory(root.path(), &session, &record).unwrap();
        assert!(
            resolved.join("owner.lock").exists(),
            "the promoted shell has no owner lease in {}",
            resolved.display()
        );
        assert_eq!(
            super::OwnerProbe::open(&resolved).unwrap().state(),
            super::OwnerState::Held,
            "TaskGet would not see a live owner for the promoted shell"
        );
        let finished = wait_until(
            root.path(),
            &session,
            id,
            "background completion",
            |value| value["status"] != "running",
        )
        .await;
        assert_eq!(finished["status"], "completed");
        let stdout = finished["stdout"].as_str().unwrap();
        assert!(
            stdout.contains("early") && stdout.contains("late"),
            "{stdout}"
        );
        // A finished promoted shell releases its lease without removing its
        // task record, so TaskGet still returns the completed output.
        let deadline = std::time::Instant::now() + READINESS_DEADLINE;
        while !matches!(
            super::OwnerProbe::open(&resolved).unwrap().state(),
            super::OwnerState::Gone(_)
        ) {
            assert!(
                std::time::Instant::now() < deadline,
                "the promoted shell kept its owner lease after it finished"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(snapshot(root.path(), &session, id)["status"], "completed");
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
        let fixture = root.path().join("managed-shell-fixture.js");
        std::fs::write(&fixture, MANAGED_SHELL_FIXTURE).unwrap();
        let own_pid = root.path().join("fixture.pid");
        let descendant_pid = root.path().join("descendant.pid");
        let arm_gate = root.path().join("descendant-arm.gate");
        let leak_gate = root.path().join("descendant-leak.gate");
        let armed = root.path().join("descendant.armed");
        let leaked = root.path().join("leaked");
        let mut owned = OwnedProcesses::new(vec![own_pid.clone(), descendant_pid.clone()]);
        let mut command = Command::new("node");
        command
            .arg(&fixture)
            .arg(&own_pid)
            .arg(&descendant_pid)
            .arg(&arm_gate)
            .arg(&leak_gate)
            .arg(&armed)
            .arg(&leaked);
        let task = scope(Some((root.path().into(), session.clone())), async {
            start(
                command,
                None,
                "node",
                "test managed shell",
                "node fixture",
                None,
                super::Interaction::None,
            )
            .unwrap()
        })
        .await;
        let id = task["task_id"].as_str().unwrap();
        // `start` only schedules the owner, and stdout/stderr are captured by
        // independent tasks, so either stream may appear first. Observe both
        // streams and the descendant marker instead of reading immediately.
        let running = wait_until(
            root.path(),
            &session,
            id,
            "live logs and a descendant",
            |value| {
                mentions(value, "stdout", "ready")
                    && mentions(value, "stderr", "diagnostic")
                    && read_pid(&descendant_pid).is_some()
            },
        )
        .await;
        assert_eq!(running["status"], "running");
        let descendant = read_pid(&descendant_pid).unwrap();
        assert!(
            !finished_process(descendant),
            "the descendant marker exists but its process is already gone"
        );
        // Prove the descendant is alive and can reach its side effect before
        // cancellation: it acknowledges an explicit arm gate. Nothing here
        // depends on how long startup took.
        std::fs::write(&arm_gate, b"arm").unwrap();
        wait_for_file(&armed, "descendant acknowledgement").await;
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
        let still_running = snapshot(root.path(), &session, id);
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
        let cancelled = wait_until(root.path(), &session, id, "cancellation", |value| {
            value["status"] != "running"
        })
        .await;
        assert_eq!(cancelled["status"], "cancelled");
        assert_eq!(cancelled["retrieval_status"], "success");
        // The owner drops the child and kills the shell's process group, but the
        // terminal status can be recorded just before that drop. Wait for the
        // exact descendant to be gone before opening the leak gate: a living one
        // writes its side effect within a single 10 ms poll.
        wait_process_finished(descendant, "cancelled descendant", &cancelled).await;
        if let Some(own) = read_pid(&own_pid) {
            wait_process_finished(own, "cancelled shell", &cancelled).await;
        }
        std::fs::write(&leak_gate, b"leak").unwrap();
        assert!(
            !leaked.exists(),
            "a cancelled descendant still produced its side effect at {}",
            leaked.display()
        );
        owned.disarm();
    }

    /// The old fixture read the logs right after `start` and required
    /// `diagnostic`. `start` only schedules the owner, so that read is
    /// legitimately empty (the old helper waited for stdout `ready` only and
    /// then demanded the independently captured stderr). The gates make the
    /// missed observation deterministic: both streams are provably empty first,
    /// then stdout appears alone, then stderr joins it.
    #[tokio::test]
    async fn early_snapshot_is_empty_until_the_gated_producer_writes() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let fixture = root.path().join("gated-log-writer.js");
        std::fs::write(&fixture, GATED_LOG_FIXTURE).unwrap();
        let pid = root.path().join("writer.pid");
        let start_gate = root.path().join("writer-start.gate");
        let stderr_gate = root.path().join("writer-stderr.gate");
        let mut owned = OwnedProcesses::new(vec![pid.clone()]);
        let mut command = Command::new("node");
        command
            .arg(&fixture)
            .arg(&pid)
            .arg(&start_gate)
            .arg(&stderr_gate);
        let task = scope(Some((root.path().into(), session.clone())), async {
            start(
                command,
                None,
                "node",
                "gated writer",
                "node fixture",
                None,
                super::Interaction::None,
            )
            .unwrap()
        })
        .await;
        let id = task["task_id"].as_str().unwrap();
        // Neither gate exists, so the producer cannot have written anything.
        let early = snapshot(root.path(), &session, id);
        assert_eq!(early["status"], "running");
        assert_eq!(early["stdout"], "", "gated producer wrote stdout: {early}");
        assert_eq!(early["stderr"], "", "gated producer wrote stderr: {early}");
        std::fs::write(&start_gate, b"start").unwrap();
        let stdout_seen = wait_until(root.path(), &session, id, "the first log stream", |value| {
            mentions(value, "stdout", "ready")
        })
        .await;
        // Negative control: this is the state in which the old assertion
        // demanded "diagnostic" and failed, and it is deterministic because the
        // producer is still blocked on its second gate.
        assert!(
            !mentions(&stdout_seen, "stderr", "diagnostic"),
            "the producer wrote stderr before its second gate: {stdout_seen}"
        );
        std::fs::write(&stderr_gate, b"write").unwrap();
        let both = wait_until(root.path(), &session, id, "both log streams", |value| {
            mentions(value, "stdout", "ready") && mentions(value, "stderr", "diagnostic")
        })
        .await;
        // The producer may exit as soon as it writes stderr, so only the two
        // streams are required here; completion is observed separately.
        assert!(mentions(&both, "stdout", "ready"));
        assert!(mentions(&both, "stderr", "diagnostic"));
        let finished = wait_until(root.path(), &session, id, "completion", |value| {
            value["status"] != "running"
        })
        .await;
        assert_eq!(finished["status"], "completed");
        assert!(
            mentions(&finished, "stdout", "ready") && mentions(&finished, "stderr", "diagnostic"),
            "the completed snapshot lost a stream: {finished}"
        );
        owned.disarm();
    }

    #[tokio::test]
    async fn managed_shell_completion_and_timeout_preserve_output() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        // Successful completion has no deadline: the producer's startup time is
        // not part of this contract, only that its output survives.
        let mut command = Command::new("node");
        command.args(["-e", "console.log('finished')"]);
        let task = scope(Some((root.path().into(), session.clone())), async {
            start(
                command,
                None,
                "node",
                "fixture",
                "node fixture",
                None,
                super::Interaction::None,
            )
            .unwrap()
        })
        .await;
        let finished = wait_until(
            root.path(),
            &session,
            task["task_id"].as_str().unwrap(),
            "completion",
            |value| value["status"] != "running",
        )
        .await;
        assert_eq!(finished["status"], "completed");
        assert!(!finished["stdout"].as_str().unwrap().is_empty());
        assert!(mentions(&finished, "stdout", "finished"));

        // A real timeout still ends the task and keeps what it wrote first. The
        // fixture writes from a shell builtin and then blocks, so the existing
        // 600 ms value is not an interpreter-startup assumption. (The polling
        // timeout, which never kills the process, is covered separately by
        // `output_wait_timeout_snapshot_and_session_isolation`.)
        #[cfg(unix)]
        let mut command = {
            let mut command = Command::new("sh");
            command.args(["-c", "printf 'partial output'; exec sleep 30"]);
            command
        };
        #[cfg(not(unix))]
        let mut command = {
            let mut command = Command::new("node");
            command.args([
                "-e",
                "process.stdout.write('partial output');setInterval(()=>{},10000)",
            ]);
            command
        };
        let task = scope(Some((root.path().into(), session.clone())), async {
            start(
                command,
                Some(Duration::from_millis(600)),
                "shell",
                "fixture",
                "shell fixture",
                None,
                super::Interaction::None,
            )
            .unwrap()
        })
        .await;
        let timed_out = wait_until(
            root.path(),
            &session,
            task["task_id"].as_str().unwrap(),
            "the timeout result",
            |value| value["status"] != "running",
        )
        .await;
        assert_eq!(timed_out["status"], "failed");
        assert!(timed_out["result"].as_str().unwrap().contains("timed_out"));
        assert!(
            mentions(&timed_out, "stdout", "partial output"),
            "the timeout lost the output written before it: {timed_out}"
        );
    }

    /// The owner record distinguishes a live owner from one whose heartbeat
    /// file is still fresh because the process was killed abruptly.
    /// A lease proves the owner is alive without trusting a reusable PID; a
    /// released lease proves it is gone; legacy/malformed records are reported
    /// as unverifiable instead of being treated as usable.
    #[cfg(unix)]
    #[test]
    fn owner_lease_identifies_alive_dead_and_legacy() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path();
        let lease = super::take_lease(directory, "fixture-task").unwrap();
        let probe = super::OwnerProbe::open(directory).unwrap();
        // Same process, different descriptor: the exclusive lease still wins.
        assert_eq!(probe.state(), super::OwnerState::Held);
        assert!(probe.state().is_held());
        // A second owner cannot take the same task's lease.
        let error = super::take_lease(directory, "fixture-task").unwrap_err();
        let message = format!("{error}");
        assert!(
            message.contains("already records an owner lock"),
            "{message}"
        );
        assert!(
            message.contains("existence alone does not prove a live owner"),
            "{message}"
        );
        drop(lease);
        assert!(
            matches!(probe.state(), super::OwnerState::Gone(_)),
            "a released lease was still treated as an owner: {:?}",
            probe.state()
        );
        assert!(!probe.state().is_held());

        // A live PID without a lease is only a legacy record: it is reported as
        // such, never as a verified owner.
        std::fs::remove_file(directory.join("owner.lock")).unwrap();
        std::fs::write(directory.join("owner.pid"), std::process::id().to_string()).unwrap();
        assert_eq!(
            super::OwnerProbe::open(directory).unwrap().state(),
            super::OwnerState::LegacyPid(std::process::id())
        );
        // A finished PID is genuinely gone even without a lease.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        child.wait().unwrap();
        std::fs::write(directory.join("owner.pid"), pid.to_string()).unwrap();
        assert!(matches!(
            super::OwnerProbe::open(directory).unwrap().state(),
            super::OwnerState::Gone(_)
        ));
        // Malformed and missing identities prove nothing.
        std::fs::write(directory.join("owner.pid"), "not-a-pid").unwrap();
        assert!(matches!(
            super::OwnerProbe::open(directory).unwrap().state(),
            super::OwnerState::Unreadable(_)
        ));
        std::fs::remove_file(directory.join("owner.pid")).unwrap();
        assert!(matches!(
            super::OwnerProbe::open(directory).unwrap().state(),
            super::OwnerState::Unreadable(_)
        ));
    }

    /// Start a background shell with the explicit stdin pipe.
    #[cfg(unix)]
    async fn pipe_shell(root: &Path, session: &str, command: Command, label: &str) -> String {
        let task = scope(Some((root.to_path_buf(), session.to_owned())), async move {
            start(
                command,
                None,
                "cat",
                label,
                "fixture",
                None,
                super::Interaction::StdinPipe,
            )
            .unwrap()
        })
        .await;
        assert_eq!(task["interaction"], "pipe", "{task}");
        assert_eq!(task["stdin_available"], true, "{task}");
        task["task_id"].as_str().unwrap().to_owned()
    }

    fn write(root: &Path, session: &str, input: Value) -> Value {
        super::super::shell_input::execute(&input, root, session)
            .unwrap_or_else(|error| panic!("TaskInput failed: {error:#}"))
    }

    fn write_error(root: &Path, session: &str, input: Value) -> String {
        let error = super::super::shell_input::execute(&input, root, session).unwrap_err();
        format!("{error}")
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn pipe_shell_echoes_utf8_messages_and_eof_completes() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let id = pipe_shell(root.path(), &session, Command::new("cat"), "echo").await;
        let first_text = "héllo 😀\n";
        let first = write(
            root.path(),
            &session,
            json!({"task_id":id,"data":first_text}),
        );
        assert_eq!(first["input_mode"], "pipe");
        assert_eq!(first["bytes_written"], first_text.len());
        assert_eq!(first["eof"], false, "{first}");
        assert_eq!(first["write_complete"], true, "{first}");
        let second_text = "second\n";
        let second = write(
            root.path(),
            &session,
            json!({"task_id":id,"data":second_text}),
        );
        assert_eq!(second["bytes_written"], second_text.len());
        let closed = write(root.path(), &session, json!({"task_id":id,"eof":true}));
        assert_eq!(closed["eof"], true, "{closed}");
        assert_eq!(closed["stdin_closed"], true, "{closed}");
        let finished = wait_until(root.path(), &session, &id, "pipe completion", |value| {
            value["status"] != "running"
        })
        .await;
        assert_eq!(finished["status"], "completed", "{finished}");
        // cat echoes exactly the bytes it read: the UTF-8 text is unchanged and
        // the two calls did not duplicate or reorder anything.
        assert_eq!(
            finished["stdout"].as_str().unwrap(),
            format!("{first_text}{second_text}")
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn repeated_eof_is_idempotent_and_writes_after_eof_fail() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let id = pipe_shell(root.path(), &session, Command::new("cat"), "closing").await;
        let first = write(root.path(), &session, json!({"task_id":id,"eof":true}));
        assert_eq!(first["already_closed"], false, "{first}");
        let again = write(root.path(), &session, json!({"task_id":id,"eof":true}));
        assert_eq!(again["already_closed"], true, "{again}");
        assert_eq!(again["stdin_closed"], true, "{again}");
        let error = write_error(root.path(), &session, json!({"task_id":id,"data":"late"}));
        assert!(error.contains("EOF was already requested"), "{error}");
        let finished = wait_until(root.path(), &session, &id, "completion", |value| {
            value["status"] != "running"
        })
        .await;
        assert_eq!(finished["status"], "completed", "{finished}");
        assert_eq!(finished["stdout"], "", "{finished}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn task_input_refuses_foreign_noninteractive_and_cancelled_tasks() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        // A normal managed shell has been offered the option but not the mode.
        let mut command = Command::new("node");
        command.args(["-e", "console.log('plain');setInterval(()=>{},10000)"]);
        let plain = scope(Some((root.path().into(), session.clone())), async {
            start(
                command,
                None,
                "node",
                "not interactive",
                "node fixture",
                None,
                super::Interaction::None,
            )
            .unwrap()
        })
        .await;
        let plain_id = plain["task_id"].as_str().unwrap();
        assert!(plain.get("interaction").is_none(), "{plain}");
        let error = write_error(
            root.path(),
            &session,
            json!({"task_id":plain_id,"data":"x"}),
        );
        assert!(error.contains("has no stdin pipe"), "{error}");
        control(
            "TaskStop",
            &json!({"task_id":plain_id}),
            root.path(),
            &session,
        )
        .unwrap();
        wait_until(root.path(), &session, plain_id, "plain stop", |value| {
            value["status"] != "running"
        })
        .await;

        let id = pipe_shell(root.path(), &session, Command::new("cat"), "owned").await;
        // Another session has no such record, so the ID alone is not authority.
        let foreign = uuid::Uuid::new_v4().to_string();
        let error = write_error(root.path(), &foreign, json!({"task_id":id,"data":"x"}));
        assert!(error.contains("Task not found"), "{error}");

        control("TaskStop", &json!({"task_id":id}), root.path(), &session).unwrap();
        let cancelled = wait_until(root.path(), &session, &id, "cancellation", |value| {
            value["status"] != "running"
        })
        .await;
        assert_eq!(cancelled["status"], "cancelled", "{cancelled}");
        let error = write_error(root.path(), &session, json!({"task_id":id,"data":"late"}));
        assert!(error.contains("is not running"), "{error}");
    }

    /// Lock probe: a held `stdin.lock` means a TaskInput call has started.
    #[cfg(unix)]
    fn lock_is_held(directory: &Path) -> bool {
        let Ok(file) = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(directory.join("stdin.lock"))
        else {
            return false;
        };
        matches!(
            crate::fs::try_lock_file(&file),
            Err(std::fs::TryLockError::WouldBlock)
        )
    }

    /// Fill the pipe until a call can no longer make progress. The returned
    /// total is exactly what the OS accepted, so a later blocked call provably
    /// has no room to write.
    #[cfg(unix)]
    fn fill_pipe(root: &Path, session: &str, id: &str) -> u64 {
        let filler = "f".repeat(super::super::shell_input::MAX_INPUT_BYTES);
        let first = write(
            root,
            session,
            json!({"task_id":id,"data":filler,"timeout":1000}),
        );
        let mut total = first["bytes_written"].as_u64().unwrap();
        assert!(
            first["write_complete"] == true || first["timed_out"] == true,
            "{first}"
        );
        // The next call cannot complete while the reader is gated: either the
        // pipe is already full (timeout) or this call fills it.
        let second = write(
            root,
            session,
            json!({"task_id":id,"data":filler,"timeout":500}),
        );
        total += second["bytes_written"].as_u64().unwrap();
        assert_eq!(second["timed_out"], true, "the pipe never filled: {second}");
        total
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_blocked_writer_reports_partial_progress_and_output_stays_readable() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let gate = root.path().join("reader.gate");
        // The process writes a ready marker, never reads stdin, and reports a
        // second marker only while the pipe is full.
        let mut command = Command::new("sh");
        command.args([
            "-c",
            &format!(
                "printf 'shell-ready\n'; while [ ! -e '{}' ]; do sleep 0.01; done; printf 'during-block\n'; exec sleep 30",
                gate.display()
            ),
        ]);
        let id = pipe_shell(root.path(), &session, command, "blocked writer").await;
        let directory = root.path().join("shell-tasks").join(&session).join(&id);
        let ready = wait_until(root.path(), &session, &id, "the ready marker", |value| {
            mentions(value, "stdout", "shell-ready")
        })
        .await;
        assert_eq!(ready["status"], "running", "{ready}");
        fill_pipe(root.path(), &session, &id);

        // A blocked write is bounded by its own timeout, not by the process.
        let pending = "p".repeat(8 * 1024);
        let started = std::time::Instant::now();
        let outcome = write(
            root.path(),
            &session,
            json!({"task_id":id,"data":pending,"timeout":300}),
        );
        assert!(started.elapsed() >= Duration::from_millis(300), "{outcome}");
        assert_eq!(outcome["timed_out"], true, "{outcome}");
        assert_eq!(outcome["write_complete"], false, "{outcome}");
        assert_eq!(outcome["is_error"], true, "{outcome}");
        let accepted = outcome["bytes_written"].as_u64().unwrap();
        assert!(accepted < pending.len() as u64, "{outcome}");
        assert_eq!(
            outcome["bytes_remaining"].as_u64().unwrap(),
            pending.len() as u64 - accepted,
            "{outcome}"
        );
        // Output written while the pipe was full is still drained and readable:
        // the write and the capture path do not block each other.
        std::fs::write(&gate, b"open").unwrap();
        let during = wait_until(
            root.path(),
            &session,
            &id,
            "the during-block marker",
            |value| value["status"] == "running" && mentions(value, "stdout", "during-block"),
        )
        .await;
        assert_eq!(during["status"], "running", "{during}");
        // A write timeout never stops the process; TaskStop is the explicit end.
        assert!(!lock_is_held(&directory));
        let stopped = control("TaskStop", &json!({"task_id":id}), root.path(), &session)
            .unwrap()
            .unwrap();
        assert_eq!(stopped["cancellation_requested"], true, "{stopped}");
        let cancelled = wait_until(root.path(), &session, &id, "stop", |value| {
            value["status"] != "running"
        })
        .await;
        assert_eq!(cancelled["status"], "cancelled", "{cancelled}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_cancelled_async_write_stops_and_sends_nothing_more() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let received = root.path().join("received.bin");
        let gate = root.path().join("reader.gate");
        // The reader stays gated (so the pipe is provably full) and drains to a
        // file only after the call has been cancelled.
        let mut command = Command::new("sh");
        command.args([
            "-c",
            &format!(
                "while [ ! -e '{}' ]; do sleep 0.01; done; cat > '{}'",
                gate.display(),
                received.display()
            ),
        ]);
        let id = pipe_shell(root.path(), &session, command, "gated reader").await;
        let directory = root.path().join("shell-tasks").join(&session).join(&id);
        let filled = fill_pipe(root.path(), &session, &id);

        // The real async entry point runs the write on the blocking pool. Its
        // cancellation guard is what this test exercises: aborting the task must
        // stop the writer, not just detach it.
        let (root_path, session_id, task_id) =
            (root.path().to_path_buf(), session.clone(), id.clone());
        let pending = "b".repeat(8 * 1024);
        let call = tokio::spawn(async move {
            crate::tools::tasks::execute_for_session_async(
                "TaskInput",
                json!({"task_id":task_id,"data":pending,"timeout":30000}),
                &root_path,
                &session_id,
            )
            .await
        });
        // Readiness is the lock the writer takes before touching the pipe.
        let deadline = std::time::Instant::now() + READINESS_DEADLINE;
        while !lock_is_held(&directory) {
            assert!(
                std::time::Instant::now() < deadline,
                "TaskInput never started writing"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        call.abort();
        let _ = call.await;
        // The writer stopped holding the lock, so it returned instead of waiting
        // for pipe capacity.
        let deadline = std::time::Instant::now() + READINESS_DEADLINE;
        while lock_is_held(&directory) {
            assert!(
                std::time::Instant::now() < deadline,
                "the cancelled writer kept holding the task stdin lock"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // Only now does the reader drain: everything it receives was accepted
        // before the cancellation, so no post-cancel bytes exist.
        std::fs::write(&gate, b"open").unwrap();
        write(root.path(), &session, json!({"task_id":id,"eof":true}));
        let finished = wait_until(root.path(), &session, &id, "reader completion", |value| {
            value["status"] != "running"
        })
        .await;
        assert_eq!(finished["status"], "completed", "{finished}");
        let length = std::fs::metadata(&received).unwrap().len();
        assert_eq!(
            length, filled,
            "the cancelled write added {length} bytes but {filled} were accepted before cancellation"
        );
    }

    /// A genuinely separate OS process writes through the real TaskInput
    /// service (`tasks::execute_for_session`) while this process owns the pipe.
    /// In child mode the same test binary, selected by `--exact`, performs the
    /// write and records the service result.
    #[cfg(unix)]
    #[tokio::test]
    async fn cross_process_writer_feeds_an_owner_in_another_process() {
        if let Ok(config) = std::env::var("TASK_INPUT_CROSS_PROCESS_CHILD") {
            let config: Value = serde_json::from_str(&config).unwrap();
            let value = crate::tools::tasks::execute_for_session(
                "TaskInput",
                config["input"].clone(),
                Path::new(config["root"].as_str().unwrap()),
                config["session"].as_str().unwrap(),
            )
            .unwrap();
            std::fs::write(config["out"].as_str().unwrap(), value.to_string()).unwrap();
            return;
        }
        crate::config::OrchestrationConfig::init_global().unwrap();
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let id = pipe_shell(root.path(), &session, Command::new("cat"), "cross process").await;
        let result_path = root.path().join("child-result.json");
        let config = json!({
            "root": root.path(),
            "session": session,
            "input": {"task_id":id,"data":"from-another-process\n"},
            "out": result_path,
        });
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tools::background_shell::tests::cross_process_writer_feeds_an_owner_in_another_process",
                "--nocapture",
            ])
            .env("TASK_INPUT_CROSS_PROCESS_CHILD", config.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(60);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the helper process did not finish"
            );
            std::thread::sleep(Duration::from_millis(20));
        };
        let mut stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            use std::io::Read;
            let _ = pipe.read_to_string(&mut stderr);
        }
        assert!(status.success(), "helper failed: {stderr}");
        let written: Value = serde_json::from_slice(&std::fs::read(&result_path).unwrap()).unwrap();
        assert_eq!(written["task_id"], id, "{written}");
        assert_eq!(
            written["bytes_written"],
            "from-another-process\n".len(),
            "{written}"
        );
        assert_eq!(written["write_complete"], true, "{written}");

        write(root.path(), &session, json!({"task_id":id,"eof":true}));
        let finished = wait_until(root.path(), &session, &id, "completion", |value| {
            value["status"] != "running"
        })
        .await;
        assert_eq!(finished["status"], "completed", "{finished}");
        assert_eq!(finished["stdout"], "from-another-process\n", "{finished}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn concurrent_writers_never_interleave_one_message() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        // `cat` echoes back, so the whole message order is observable.
        let id = pipe_shell(root.path(), &session, Command::new("cat"), "concurrent").await;
        let message = |byte: char| format!("{}", byte).repeat(8192);
        let (a, b) = (message('a'), message('b'));
        let writers = [
            (
                a.clone(),
                root.path().to_path_buf(),
                session.clone(),
                id.clone(),
            ),
            (
                b.clone(),
                root.path().to_path_buf(),
                session.clone(),
                id.clone(),
            ),
        ]
        .into_iter()
        .map(|(text, root, session, task)| {
            std::thread::spawn(move || {
                super::super::shell_input::execute(
                    &json!({"task_id":task,"data":text}),
                    &root,
                    &session,
                )
                .unwrap()
            })
        })
        .collect::<Vec<_>>();
        for writer in writers {
            let value = writer.join().unwrap();
            assert_eq!(value["write_complete"], true, "{value}");
            assert_eq!(value["bytes_written"], 8192, "{value}");
        }
        write(root.path(), &session, json!({"task_id":id,"eof":true}));
        let finished = wait_until(root.path(), &session, &id, "completion", |value| {
            value["status"] != "running"
        })
        .await;
        assert_eq!(finished["status"], "completed", "{finished}");
        let stdout = finished["stdout"].as_str().unwrap();
        assert_eq!(stdout.len(), 16384, "{stdout:?}");
        // Either order is fine; an interleaved write is not.
        assert!(
            stdout == format!("{a}{b}") || stdout == format!("{b}{a}"),
            "messages interleaved"
        );
    }

    /// A managed pipe shell whose lease is held by the test itself, because an
    /// in-process owner task cannot die on demand. The task record, FIFO,
    /// keepalive and reading process are all real.
    #[cfg(unix)]
    struct GatedPipe {
        root: tempfile::TempDir,
        session: String,
        id: String,
        directory: PathBuf,
        lease: Option<super::OwnerLease>,
        keepalive: Option<std::fs::File>,
        reader: Option<std::process::Child>,
        gate: PathBuf,
        received: PathBuf,
    }

    #[cfg(unix)]
    impl GatedPipe {
        /// `script` runs with `$1` = gate path and `$2` = received path and gets
        /// the real FIFO as stdin.
        fn new(script: &str) -> Self {
            let root = tempfile::tempdir().unwrap();
            let session = uuid::Uuid::new_v4().to_string();
            let task = super::super::tasks::execute_for_shell_owner(
                "TaskCreate",
                json!({"subject":"gated pipe","metadata":{"managed_shell":true,"interaction":"pipe"}}),
                root.path(),
                &session,
            )
            .unwrap();
            let id = task["id"].as_str().unwrap().to_owned();
            let directory = root.path().join("shell-tasks").join(&session).join(&id);
            std::fs::create_dir_all(&directory).unwrap();
            let lease = super::take_lease(&directory, &id).unwrap();
            std::fs::write(directory.join("owner.pid"), std::process::id().to_string()).unwrap();
            std::fs::write(directory.join("heartbeat"), b"running").unwrap();
            super::super::tasks::execute_for_shell_owner(
                "TaskUpdate",
                json!({"id":id,"status":"running"}),
                root.path(),
                &session,
            )
            .unwrap();
            let (child_stdin, keepalive) = super::prepare_stdin_pipe(&directory, &id).unwrap();
            let gate = root.path().join("reader.gate");
            let received = root.path().join("received.bin");
            let mut command = std::process::Command::new("sh");
            command
                .arg("-c")
                .arg(script)
                .arg("fixture-reader")
                .arg(&gate)
                .arg(&received)
                .stdin(std::process::Stdio::from(child_stdin))
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            Self {
                root,
                session,
                id,
                directory,
                lease: Some(lease),
                keepalive: Some(keepalive),
                reader: Some(command.spawn().unwrap()),
                gate,
                received,
            }
        }

        fn write(&self, input: Value) -> Result<Value> {
            super::super::shell_input::execute(&input, self.root.path(), &self.session)
        }

        /// Release the owner lease exactly now; every later check must see it.
        fn lose_owner(&mut self) {
            self.lease.take();
        }

        fn reader_pid(&self) -> u32 {
            self.reader.as_ref().unwrap().id()
        }
    }

    #[cfg(unix)]
    impl Drop for GatedPipe {
        fn drop(&mut self) {
            // Exact owned processes only: open the gate so a blocked reader can
            // finish, then kill and reap the child this fixture started.
            let _ = std::fs::write(&self.gate, b"open");
            self.keepalive.take();
            if let Some(mut reader) = self.reader.take() {
                let _ = reader.kill();
                let _ = reader.wait();
            }
        }
    }

    /// Waiting for the per-task stdin lock must end when the owner dies, across
    /// processes. This test holds the lock, and the helper publishes a marker
    /// from the real `TryLockError::WouldBlock` arm, which is reached only after
    /// its own live-owner validation: the parent therefore loses the owner while
    /// the helper is provably in the lock wait and not in the earlier checks.
    #[cfg(unix)]
    #[tokio::test]
    async fn owner_loss_stops_a_cross_process_write_waiting_for_the_lock() {
        if let Ok(config) = std::env::var("TASK_INPUT_OWNER_LOSS_CHILD") {
            let config: Value = serde_json::from_str(&config).unwrap();
            // The parent may only lose the owner once this process is provably
            // inside the stdin.lock wait. The marker is written once from the
            // production `WouldBlock` arm; it is not a timing guess.
            let ready = PathBuf::from(config["ready"].as_str().unwrap());
            let mut fired = false;
            super::super::shell_input::capacity_barrier::install_lock_wait(Box::new(move |_| {
                if fired {
                    return;
                }
                fired = true;
                let _ = std::fs::write(&ready, b"lock-wait");
            }));
            let result = super::super::shell_input::execute(
                &config["input"],
                Path::new(config["root"].as_str().unwrap()),
                config["session"].as_str().unwrap(),
            );
            super::super::shell_input::capacity_barrier::clear_lock_wait();
            let value = match result {
                Ok(value) => json!({"ok": true, "value": value}),
                Err(error) => json!({"ok": false, "error": format!("{error:#}")}),
            };
            std::fs::write(config["out"].as_str().unwrap(), value.to_string()).unwrap();
            return;
        }
        crate::config::OrchestrationConfig::init_global().unwrap();
        let mut pipe = GatedPipe::new("exec cat > \"$2\"");
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(pipe.directory.join("stdin.lock"))
            .unwrap();
        crate::fs::lock_file(&lock).unwrap();
        let out = pipe.root.path().join("child-result.json");
        let ready = pipe.root.path().join("child-lock-wait");
        let config = json!({
            "root": pipe.root.path(),
            "session": pipe.session,
            "input": {"task_id":pipe.id,"data":"blocked-bytes","timeout":60000},
            "out": out,
            "ready": ready,
        });
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tools::background_shell::tests::owner_loss_stops_a_cross_process_write_waiting_for_the_lock",
                "--nocapture",
            ])
            .env("TASK_INPUT_OWNER_LOSS_CHILD", config.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        // The helper is inside the production lock-wait arm, past the initial
        // owner validation, before the owner is lost.
        let deadline = std::time::Instant::now() + READINESS_DEADLINE;
        while !ready.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "the helper never reached the task stdin lock wait"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        pipe.lose_owner();
        let status = tokio::time::timeout(READINESS_DEADLINE, async {
            tokio::task::spawn_blocking(move || child.wait())
                .await
                .unwrap()
        })
        .await
        .expect("the helper kept waiting without an owner");
        let status = status.unwrap();
        assert!(status.success(), "the helper failed: {status}");
        let result: Value = serde_json::from_slice(&std::fs::read(&out).unwrap()).unwrap();
        assert_eq!(result["ok"], true, "{result}");
        let value = &result["value"];
        assert_eq!(value["owner_lost"], true, "{result}");
        assert_eq!(value["bytes_written"], 0, "{result}");
        assert_eq!(value["write_complete"], false, "{result}");
        assert_eq!(value["is_error"], true, "{result}");
        assert!(
            value["error"]
                .as_str()
                .unwrap_or_default()
                .contains("owner lease is released"),
            "{result}"
        );
        assert!(
            !pipe.directory.join("stdin-eof").exists(),
            "an ownerless call wrote EOF"
        );
        assert_eq!(
            std::fs::metadata(&pipe.received)
                .map(|meta| meta.len())
                .unwrap_or(0),
            0,
            "bytes reached the reader after the owner was lost"
        );
    }

    /// A transient TaskInput writer descriptor must not be inherited by an
    /// unrelated process: an inherited copy keeps the shell's stdin pipe open
    /// after the owner closes its own, so the shell never sees EOF.
    ///
    /// The unrelated child is started while a real write holds the FIFO writer;
    /// then the write is allowed to time out, stdin is closed, and the shell must
    /// still reach EOF and complete while that child is alive.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_transient_writer_descriptor_is_not_inherited_by_unrelated_children() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let gate = root.path().join("reader.gate");
        let received = root.path().join("received.bin");
        let mut command = Command::new("sh");
        command.args([
            "-c",
            &format!(
                "while [ ! -e '{}' ]; do sleep 0.01; done; cat > '{}'",
                gate.display(),
                received.display()
            ),
        ]);
        let id = pipe_shell(root.path(), &session, command, "inheritance").await;
        let directory = root.path().join("shell-tasks").join(&session).join(&id);
        fill_pipe(root.path(), &session, &id);

        let (root_path, session_id, task_id) =
            (root.path().to_path_buf(), session.clone(), id.clone());
        let writer = std::thread::spawn(move || {
            super::super::shell_input::execute(
                &json!({"task_id":task_id,"data":"i".repeat(8 * 1024),"timeout":500}),
                &root_path,
                &session_id,
            )
            .unwrap()
        });
        let deadline = std::time::Instant::now() + READINESS_DEADLINE;
        while !lock_is_held(&directory) {
            assert!(
                std::time::Instant::now() < deadline,
                "the write never started"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // Started while the transient writer descriptor exists.
        let mut unrelated = std::process::Command::new("sh")
            .args(["-c", "sleep 30"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        struct Reap(u32);
        impl Drop for Reap {
            fn drop(&mut self) {
                unsafe extern "C" {
                    fn kill(pid: i32, signal: i32) -> i32;
                }
                if self.0 > 0 && self.0 <= i32::MAX as u32 {
                    unsafe {
                        kill(self.0 as i32, 9);
                    }
                }
            }
        }
        let _reap = Reap(unrelated.id());
        #[cfg(target_os = "linux")]
        {
            let inherited = std::fs::read_dir(format!("/proc/{}/fd", unrelated.id()))
                .unwrap()
                .flatten()
                .filter(|entry| {
                    std::fs::read_link(entry.path())
                        .is_ok_and(|target| target.ends_with("stdin.pipe"))
                })
                .count();
            let _ = &mut unrelated;
            assert_eq!(
                inherited, 0,
                "the unrelated process inherited {inherited} TaskInput pipe descriptor(s)"
            );
        }
        // The blocked write ends by its own timeout, then stdin closes.
        let outcome = writer.join().unwrap();
        assert_eq!(outcome["timed_out"], true, "{outcome}");
        std::fs::write(&gate, b"open").unwrap();
        write(root.path(), &session, json!({"task_id":id,"eof":true}));
        let finished = wait_until(root.path(), &session, &id, "shell EOF", |value| {
            value["status"] != "running"
        })
        .await;
        assert_eq!(finished["status"], "completed", "{finished}");
        assert!(
            std::fs::metadata(&received).is_ok_and(|meta| meta.len() > 0),
            "the shell never received its stdin bytes"
        );
        let _ = &mut unrelated;
    }

    /// A write blocked on a full pipe stops at owner loss and the reader sees
    /// exactly the bytes accepted before it.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_full_pipe_write_stops_at_owner_loss() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let mut pipe = GatedPipe::new("while [ ! -e \"$1\" ]; do sleep 0.01; done; cat > \"$2\"");
        let filled = fill_pipe(pipe.root.path(), &pipe.session, &pipe.id);
        assert!(filled > 0, "the pipe never filled");

        let (root, session, id) = (
            pipe.root.path().to_owned(),
            pipe.session.clone(),
            pipe.id.clone(),
        );
        let waiting = std::thread::spawn(move || {
            super::super::shell_input::execute(
                &json!({"task_id":id,"data":"p".repeat(8 * 1024),"timeout":60000}),
                &root,
                &session,
            )
            .unwrap()
        });
        // Readiness: the blocked call holds the per-task stdin lock while the
        // pipe has no room (proved by the fill timeouts above).
        let deadline = std::time::Instant::now() + READINESS_DEADLINE;
        while !lock_is_held(&pipe.directory) {
            assert!(
                std::time::Instant::now() < deadline,
                "the blocked write never took the task stdin lock"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        pipe.lose_owner();
        let started = std::time::Instant::now();
        let outcome = waiting.join().unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "a blocked write without an owner kept waiting: {outcome}"
        );
        assert_eq!(outcome["owner_lost"], true, "{outcome}");
        assert_eq!(outcome["write_complete"], false, "{outcome}");
        assert_eq!(
            outcome["bytes_written"], 0,
            "a full pipe accepted bytes after owner loss: {outcome}"
        );
        assert!(
            !pipe.directory.join("stdin-eof").exists(),
            "an ownerless blocked write wrote EOF"
        );
        // The reader now drains exactly the pre-existing bytes.
        std::fs::write(&pipe.gate, b"open").unwrap();
        let deadline = std::time::Instant::now() + READINESS_DEADLINE;
        loop {
            let length = std::fs::metadata(&pipe.received)
                .map(|meta| meta.len())
                .unwrap_or(0);
            if length == filled {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the reader received {length} bytes, expected exactly {filled}"
            );
            assert!(
                length < filled,
                "the reader received {length} bytes but only {filled} were accepted"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // Closing the keepalive gives the still-live reader EOF.
        pipe.keepalive.take();
        let deadline = std::time::Instant::now() + READINESS_DEADLINE;
        while pipe.reader.as_mut().unwrap().try_wait().unwrap().is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "the reader did not exit after its writer closed"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            std::fs::metadata(&pipe.received).unwrap().len(),
            filled,
            "the aborted write added bytes the reader consumed"
        );
    }

    /// The shell must not inherit the owner lease: otherwise killing the owner
    /// would not release it while the shell is still alive.
    #[cfg(unix)]
    #[tokio::test]
    async fn owner_lease_is_not_inherited_by_the_shell() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let pid_file = root.path().join("shell.pid");
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(format!("echo $$ > '{}'; exec sleep 30", pid_file.display()));
        let task = scope(Some((root.path().into(), session.clone())), async {
            start(
                command,
                None,
                "sh",
                "lease inheritance",
                "sh fixture",
                None,
                super::Interaction::None,
            )
            .unwrap()
        })
        .await;
        let id = task["task_id"].as_str().unwrap();
        let directory = root.path().join("shell-tasks").join(&session).join(id);
        assert_eq!(
            super::OwnerProbe::open(&directory).unwrap().state(),
            super::OwnerState::Held
        );
        // The recorded pid is the shell, which must not hold the lease fd.
        let deadline = std::time::Instant::now() + READINESS_DEADLINE;
        while !pid_file.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "the shell never recorded its PID"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        #[cfg(target_os = "linux")]
        {
            let pid: u32 = std::fs::read_to_string(&pid_file)
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            let entries = std::fs::read_dir(format!("/proc/{pid}/fd")).unwrap();
            for entry in entries.flatten() {
                let target = std::fs::read_link(entry.path()).unwrap_or_default();
                assert!(
                    !target.ends_with("owner.lock"),
                    "the shell inherited the owner lease: {}",
                    target.display()
                );
            }
        }
        control("TaskStop", &json!({"task_id":id}), root.path(), &session).unwrap();
        let stopped = wait_until(root.path(), &session, id, "stop", |value| {
            value["status"] != "running"
        })
        .await;
        assert_eq!(stopped["status"], "cancelled", "{stopped}");
        // Once the owner task ends the lease is released without any cleanup.
        let deadline = std::time::Instant::now() + READINESS_DEADLINE;
        loop {
            let state = super::OwnerProbe::open(&directory).unwrap().state();
            if matches!(state, super::OwnerState::Gone(_)) {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the owner lease outlived its owner task: {state:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Publishing the task before taking the lease must not let a concurrent
    /// probe steal it: observers run continuously across publication and while
    /// the lease is held, and must never see the lease as released. They are
    /// stopped and joined *before* the owner is dropped, so the released state
    /// is asserted separately rather than by the observers.
    #[cfg(unix)]
    #[test]
    fn observers_never_see_a_released_lease_while_the_owner_lives() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::Arc;
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().to_path_buf();
        let stop = Arc::new(AtomicBool::new(false));
        let held = Arc::new(AtomicUsize::new(0));
        let observers = (0..4)
            .map(|_| {
                let directory = directory.clone();
                let stop = stop.clone();
                let held = held.clone();
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Acquire) {
                        let Ok(probe) = super::OwnerProbe::open(&directory) else {
                            continue;
                        };
                        match probe.state() {
                            super::OwnerState::Held => {
                                held.fetch_add(1, Ordering::AcqRel);
                            }
                            // The record is published by a link, so a probe that
                            // ran before it is a legacy/absent record, not a
                            // released lease.
                            super::OwnerState::Unreadable(_) => {}
                            other => panic!("a live owner's lease appeared as {other:?}"),
                        }
                    }
                })
            })
            .collect::<Vec<_>>();
        // Take the lease while the observers are already running.
        let lease = super::take_lease(&directory, "race").unwrap();
        let deadline = std::time::Instant::now() + READINESS_DEADLINE;
        while held.load(Ordering::Acquire) < 20 {
            assert!(
                std::time::Instant::now() < deadline,
                "the observers never saw the live lease"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            super::OwnerProbe::open(&directory).unwrap().state(),
            super::OwnerState::Held
        );
        stop.store(true, Ordering::Release);
        for observer in observers {
            observer.join().unwrap();
        }
        // Released state, asserted after the observers have stopped.
        drop(lease);
        assert!(matches!(
            super::OwnerProbe::open(&directory).unwrap().state(),
            super::OwnerState::Gone(_)
        ));
    }

    /// A second owner for the same task must not replace the live lease record.
    #[cfg(unix)]
    #[test]
    fn a_second_owner_cannot_replace_a_live_lease() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path();
        let lease = super::take_lease(directory, "dup").unwrap();
        let error = super::take_lease(directory, "dup").unwrap_err();
        let message = format!("{error}");
        assert!(
            message.contains("already records an owner lock"),
            "{message}"
        );
        assert!(
            message.contains("existence alone does not prove a live owner"),
            "{message}"
        );
        // The live record is still the first owner's: it reports Held now and
        // becomes Gone only when that owner drops it.
        assert_eq!(
            super::OwnerProbe::open(directory).unwrap().state(),
            super::OwnerState::Held
        );
        drop(lease);
        assert!(matches!(
            super::OwnerProbe::open(directory).unwrap().state(),
            super::OwnerState::Gone(_)
        ));
    }

    /// Legacy and malformed ownership cannot authorize a write, while TaskGet
    /// stays readable.
    #[cfg(unix)]
    #[tokio::test]
    async fn legacy_and_malformed_ownership_refuse_input_but_task_get_stays_readable() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let task = super::super::tasks::execute_for_shell_owner(
            "TaskCreate",
            json!({"subject":"legacy owner","metadata":{"managed_shell":true,"interaction":"pipe"}}),
            root.path(),
            &session,
        )
        .unwrap();
        let id = task["id"].as_str().unwrap().to_owned();
        let directory = root.path().join("shell-tasks").join(&session).join(&id);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("stdout.log"), b"legacy output\n").unwrap();
        std::fs::write(directory.join("heartbeat"), b"running").unwrap();
        super::super::tasks::execute_for_shell_owner(
            "TaskUpdate",
            json!({"id":id,"status":"running"}),
            root.path(),
            &session,
        )
        .unwrap();

        // A live PID is not an owner lease.
        std::fs::write(directory.join("owner.pid"), std::process::id().to_string()).unwrap();
        let observed = snapshot(root.path(), &session, &id);
        assert_eq!(observed["status"], "running", "{observed}");
        assert_eq!(observed["stdout"], "legacy output\n", "{observed}");
        let error = super::super::shell_input::execute(
            &json!({"task_id":id,"data":"nope"}),
            root.path(),
            &session,
        )
        .unwrap_err();
        assert!(format!("{error}").contains("no owner lease"), "{error}");

        // Malformed identity is not permission either.
        std::fs::write(directory.join("owner.pid"), "not-a-pid").unwrap();
        let error = super::super::shell_input::execute(
            &json!({"task_id":id,"data":"nope"}),
            root.path(),
            &session,
        )
        .unwrap_err();
        assert!(
            format!("{error}").contains("owner PID is malformed"),
            "{error}"
        );
        // TaskGet still reads the record and its logs.
        let observed = snapshot(root.path(), &session, &id);
        assert_eq!(observed["stdout"], "legacy output\n", "{observed}");
    }
    /// A real `start` whose session directory cannot be created still has to
    /// leave an honest, terminal record for the task TaskCreate already made.
    #[tokio::test]
    async fn failed_start_records_the_initialization_error_instead_of_pending() {
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        // A regular file where the session task directory belongs makes
        // create_dir_all fail after TaskCreate inserted the record.
        std::fs::create_dir_all(root.path().join("shell-tasks")).unwrap();
        std::fs::write(
            root.path().join("shell-tasks").join(&session),
            b"not a directory",
        )
        .unwrap();
        let command = Command::new("sh");
        let error = scope(Some((root.path().into(), session.clone())), async move {
            start(
                command,
                None,
                "sh",
                "cannot start",
                "sh -c true",
                None,
                super::Interaction::None,
            )
        })
        .await
        .unwrap_err();
        assert!(!format!("{error:#}").trim().is_empty());
        let tasks =
            super::super::tasks::execute_for_session("TaskList", json!({}), root.path(), &session)
                .unwrap();
        let tasks = tasks["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 1, "{tasks:?}");
        assert_eq!(tasks[0]["status"], "failed", "{:?}", tasks[0]);
        let output = tasks[0]["output"].as_str().unwrap_or_default();
        assert!(
            output.contains("startup_failed") && output.contains("directory"),
            "the record does not name the cause: {output}"
        );
        // The failed record stays readable through the ordinary read path.
        let id = tasks[0]["id"].as_str().unwrap();
        assert_eq!(snapshot(root.path(), &session, id)["status"], "failed");
    }

    /// Dropping the owner lease must release the lock explicitly. A duplicated
    /// descriptor shares the open file description, so closing only the owner's
    /// handle would leave the flock held as if the owner were still alive.
    #[cfg(unix)]
    #[test]
    fn dropping_the_owner_lease_releases_a_shared_description() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path();
        let lease = super::take_lease(directory, "shared-lease").unwrap();
        // `try_clone` uses `dup`, the same open file description a fork shares.
        let duplicate = lease.0.as_ref().unwrap().try_clone().unwrap();
        let probe = super::OwnerProbe::open(directory).unwrap();
        assert_eq!(probe.state(), super::OwnerState::Held);
        drop(lease);
        assert!(
            matches!(probe.state(), super::OwnerState::Gone(_)),
            "a dropped owner kept its shared-description lease: {:?}",
            probe.state()
        );
        // Explicit release does not delete the identity record.
        assert!(directory.join("owner.lock").exists());
        drop(duplicate);
    }

    /// A startup that cannot take its lease records the failure, keeps the
    /// conflicting record byte for byte, and leaves no staging file behind.
    #[cfg(unix)]
    #[test]
    fn failed_start_lease_collision_preserves_the_existing_owner_record() {
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let task = super::super::tasks::execute_for_shell_owner(
            "TaskCreate",
            json!({"subject":"lease conflict","metadata":{"managed_shell":true}}),
            root.path(),
            &session,
        )
        .unwrap();
        let id = task["id"].as_str().unwrap().to_owned();
        let directory = root.path().join("shell-tasks").join(&session).join(&id);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("owner.lock"), b"historical owner record").unwrap();
        std::fs::write(directory.join("owner.pid"), b"424242").unwrap();
        let error = super::initialize_start(
            root.path(),
            &session,
            &id,
            &directory,
            super::Interaction::None,
        )
        .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("already records an owner lock"),
            "{message}"
        );
        // Neither the identity nor the bytes of the existing record changed.
        assert_eq!(
            std::fs::read(directory.join("owner.lock")).unwrap(),
            b"historical owner record"
        );
        assert_eq!(
            std::fs::read(directory.join("owner.pid")).unwrap(),
            b"424242"
        );
        assert!(
            std::fs::read_dir(&directory)
                .unwrap()
                .flatten()
                .all(|entry| !entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("owner.lock.new-")),
            "the failed acquisition left its staging file behind"
        );
        // An unlocked record is free, not proof of a live owner.
        assert!(matches!(
            super::OwnerProbe::open(&directory).unwrap().state(),
            super::OwnerState::Gone(_)
        ));
        let record = super::super::tasks::read_for_session(&id, root.path(), &session).unwrap();
        assert_eq!(record["status"], "failed", "{record}");
        let output = record["output"].as_str().unwrap_or_default();
        assert!(output.contains("startup_failed"), "{output}");
        assert!(output.contains("already records an owner lock"), "{output}");
    }

    /// A heartbeat failure happens after the running update. The task must not
    /// stay "running" with no owner, and the helper must release its own lease.
    #[cfg(unix)]
    #[test]
    fn failed_start_after_running_update_records_failure_and_releases_the_lease() {
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let task = super::super::tasks::execute_for_shell_owner(
            "TaskCreate",
            json!({"subject":"heartbeat failure","metadata":{"managed_shell":true}}),
            root.path(),
            &session,
        )
        .unwrap();
        let id = task["id"].as_str().unwrap().to_owned();
        let directory = root.path().join("shell-tasks").join(&session).join(&id);
        std::fs::create_dir_all(&directory).unwrap();
        // A directory where the heartbeat file belongs makes the write fail
        // after TaskUpdate already stored "running", with no chmod involved.
        std::fs::create_dir(directory.join("heartbeat")).unwrap();
        let error = super::initialize_start(
            root.path(),
            &session,
            &id,
            &directory,
            super::Interaction::None,
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("heartbeat"),
            "unexpected error: {error:#}"
        );
        let record = super::super::tasks::read_for_session(&id, root.path(), &session).unwrap();
        assert_eq!(record["status"], "failed", "{record}");
        assert_ne!(record["status"], "running");
        let output = record["output"].as_str().unwrap_or_default();
        assert!(output.contains("startup_failed"), "{output}");
        // The helper owns this lease, so it must have released it on failure.
        assert!(matches!(
            super::OwnerProbe::open(&directory).unwrap().state(),
            super::OwnerState::Gone(_)
        ));
    }

    /// A terminal update that also fails must not be swallowed: the caller sees
    /// the original cause and the recording failure, never a claim of a saved
    /// failed record.
    #[test]
    fn unrecordable_startup_failure_reports_both_errors() {
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let task = super::super::tasks::execute_for_shell_owner(
            "TaskCreate",
            json!({"subject":"unrecordable failure","metadata":{"managed_shell":true}}),
            root.path(),
            &session,
        )
        .unwrap();
        let id = task["id"].as_str().unwrap().to_owned();
        let database = root.path().join("tasks").join(format!("{session}.sqlite3"));
        std::fs::rename(&database, database.with_extension("original")).unwrap();
        std::fs::create_dir(&database).unwrap();
        let error = super::fail_startup(
            root.path(),
            &session,
            &id,
            anyhow::anyhow!("original initialization cause"),
        );
        let message = format!("{error:#}");
        assert!(
            message.contains("original initialization cause"),
            "{message}"
        );
        assert!(
            message.contains("Recording the failure also failed"),
            "{message}"
        );
        assert!(message.contains("previous status"), "{message}");
    }

    /// Starts a foreground call whose process tree records its own PIDs and
    /// prints a marker into its captured stdout, so a failed promotion can be
    /// checked against both the exact processes it owned and the output it had
    /// already captured.
    const PROMOTION_FIXTURE: &str = r#"
const fs = require('fs');
const { spawn } = require('child_process');
const [ownPid, descendantPid, started] = process.argv.slice(2);
const descendant = `
const fs = require('fs');
fs.writeFileSync(process.argv[1], String(process.pid));
setTimeout(() => {}, 600000);
`;
spawn(process.execPath, ['-e', descendant, descendantPid], { stdio: 'inherit' });
fs.writeFileSync(ownPid, String(process.pid));
fs.writeFileSync(started, 'started');
console.log('promotion-ready');
setInterval(() => {}, 1000);
"#;

    /// A promotion that cannot take its owner lease must record the failure and
    /// stop the foreground process tree it already owns. A pre-existing owner
    /// record is not replaced, and its bytes are left alone. The log already
    /// captured before the failure is kept too.
    #[tokio::test]
    async fn failed_foreground_promotion_records_failure_and_stops_the_process_tree() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let fixture = root.path().join("promotion-fixture.js");
        std::fs::write(&fixture, PROMOTION_FIXTURE).unwrap();
        let own_pid = root.path().join("promotion-own.pid");
        let descendant_pid = root.path().join("promotion-descendant.pid");
        let started = root.path().join("promotion-started");
        let mut owned = OwnedProcesses::new(vec![own_pid.clone(), descendant_pid.clone()]);
        let mut command = Command::new("node");
        command
            .arg(&fixture)
            .arg(&own_pid)
            .arg(&descendant_pid)
            .arg(&started);
        let root_path = root.path().to_owned();
        let session_id = session.clone();
        let signal_own = own_pid.clone();
        let signal_descendant = descendant_pid.clone();
        let signal_started = started.clone();
        let (result, directory) = scope(Some((root.path().into(), session.clone())), async move {
            let request = async move {
                // Wait until the call really owns both processes, then plant
                // a historical owner record where the promotion would
                // publish its lease. Promotion must fail rather than
                // replace that record.
                wait_for_file(&signal_own, "foreground root pid").await;
                wait_for_file(&signal_descendant, "foreground descendant pid").await;
                wait_for_file(&signal_started, "foreground fixture startup").await;
                let session_directory = root_path.join("shell-tasks").join(&session_id);
                let directory = std::fs::read_dir(&session_directory)
                    .unwrap()
                    .flatten()
                    .map(|entry| entry.path())
                    .find(|path| {
                        path.file_name()
                            .and_then(|name| name.to_str())
                            .is_some_and(|name| name.starts_with("fg-"))
                            && path.join("foreground-active").exists()
                    })
                    .expect("the foreground log directory is discoverable");
                // The captured output must really exist before the failure,
                // so keeping it afterwards proves something.
                wait_for_log_contains(
                    &directory.join("stdout.log"),
                    "promotion-ready",
                    "captured foreground output",
                )
                .await;
                std::fs::write(directory.join("owner.lock"), b"historical owner record").unwrap();
                request_background(&root_path, &session_id).unwrap();
                directory
            };
            let (result, directory) = tokio::join!(
                run_foreground(
                    command,
                    Duration::from_secs(30),
                    "node",
                    "promotion",
                    "node promotion",
                    None
                ),
                request
            );
            (result, directory)
        })
        .await;
        let error = result.unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("owner lock"), "{message}");
        assert_eq!(
            std::fs::read(directory.join("owner.lock")).unwrap(),
            b"historical owner record"
        );
        // Aborting must not discard the logs captured before the failure.
        let captured = std::fs::read_to_string(directory.join("stdout.log"))
            .expect("the captured stdout log was removed");
        assert!(captured.contains("promotion-ready"), "{captured}");
        let own = read_pid(&own_pid).unwrap();
        let descendant = read_pid(&descendant_pid).unwrap();
        wait_process_finished(own, "aborted foreground root", &message).await;
        wait_process_finished(descendant, "aborted foreground descendant", &message).await;
        owned.disarm();
        let tasks =
            super::super::tasks::execute_for_session("TaskList", json!({}), root.path(), &session)
                .unwrap();
        let tasks = tasks["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 1, "{tasks:?}");
        assert_eq!(tasks[0]["status"], "failed", "{:?}", tasks[0]);
        let output = tasks[0]["output"].as_str().unwrap_or_default();
        assert!(output.contains("startup_failed"), "{output}");
    }

    // ---- Linux owner liveness: identity recorded inside the lease ----

    /// Node shell: records its PID, reports readiness through the production
    /// stdout capture, and never reads stdin, so its FIFO read end stays open
    /// with no consumer.
    const LINUX_OWNER_SHELL: &str = r#"
const fs = require('fs');
fs.writeFileSync(process.argv[2], String(process.pid));
console.log('ready');
setInterval(() => {}, 1000);
"#;

    /// Node helper: proves it inherited a usable descriptor, records itself, and
    /// keeps that descriptor open for as long as it lives.
    const LINUX_OWNER_HELPER: &str = r#"
const fs = require('fs');
const fd = Number(process.env.DCL30_FD);
const stat = fs.fstatSync(fd);
if (!stat || stat.ino === 0) { process.exit(3); }
fs.writeFileSync(process.argv[2], String(process.pid));
fs.writeFileSync(process.argv[3], 'helper-ready');
setInterval(() => {}, 1000);
"#;

    /// Owner role: a separate OS process that really runs `scope`/`start` with
    /// `interaction:"pipe"`, duplicates the descriptor it holds for the task's
    /// lease, hands the duplicate (made inheritable) to a helper, and stays alive
    /// until the parent kills it.
    async fn linux_owner_role() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let root = PathBuf::from(std::env::var("DCL30_ROOT").unwrap());
        let session = std::env::var("DCL30_SESSION").unwrap();
        let fixture = PathBuf::from(std::env::var("DCL30_FIXTURE").unwrap());
        let shell_script = fixture.join("shell.js");
        let helper_script = fixture.join("helper.js");
        std::fs::write(&shell_script, LINUX_OWNER_SHELL).unwrap();
        std::fs::write(&helper_script, LINUX_OWNER_HELPER).unwrap();
        let mut command = Command::new("node");
        command.arg(&shell_script).arg(fixture.join("shell.pid"));
        let started = scope(Some((root.clone(), session.clone())), async move {
            start(
                command,
                None,
                "node",
                "owned pipe shell",
                "node owned-pipe",
                None,
                super::Interaction::StdinPipe,
            )
        })
        .await
        .unwrap();
        let id = started["task_id"].as_str().unwrap().to_owned();
        let directory = root.join("shell-tasks").join(&session).join(&id);
        // `start` returns before the shell is exec'd. The probe copy below is
        // deliberately inheritable, so a process spawned by this owner afterwards
        // would wrongly inherit it; wait until the real shell exists first.
        let shell_pid = fixture.join("shell.pid");
        for _ in 0..600 {
            if shell_pid.exists()
                && std::fs::read_to_string(directory.join("stdout.log"))
                    .is_ok_and(|text| text.contains("ready"))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            shell_pid.exists(),
            "the pipe shell never recorded its PID at {}",
            shell_pid.display()
        );
        // Find the descriptor this process really holds for the task's lease by
        // inode: `take_lease` publishes with a hard link and unlinks its staging
        // name. Opening the lock file again would create a different open file
        // description that does not share the flock.
        use std::os::unix::fs::MetadataExt;
        let lease_meta = std::fs::metadata(directory.join("owner.lock")).unwrap();
        let source = std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .flatten()
            .find_map(|entry| {
                let number = entry.file_name().to_string_lossy().parse::<i32>().ok()?;
                let meta = std::fs::metadata(entry.path()).ok()?;
                (meta.dev() == lease_meta.dev() && meta.ino() == lease_meta.ino()).then_some(number)
            })
            .expect("no descriptor in the owner holds this task's lease");
        unsafe extern "C" {
            fn dup(fd: i32) -> i32;
            fn fcntl(fd: i32, command: i32, ...) -> i32;
        }
        const F_GETFD: i32 = 1;
        const F_SETFD: i32 = 2;
        const FD_CLOEXEC: i32 = 1;
        let duplicate = unsafe { dup(source) };
        assert!(duplicate >= 0, "duplicating the lease descriptor failed");
        let flags = unsafe { fcntl(duplicate, F_GETFD) };
        assert!(
            flags >= 0,
            "reading the duplicate's descriptor flags failed"
        );
        // Only this probe copy becomes inheritable; the product's lease
        // descriptor keeps its own flags.
        assert!(
            unsafe { fcntl(duplicate, F_SETFD, flags & !FD_CLOEXEC) } >= 0,
            "clearing CLOEXEC on the probe copy failed"
        );
        let mut helper = std::process::Command::new("node");
        helper
            .arg(&helper_script)
            .arg(fixture.join("helper.pid"))
            .arg(fixture.join("helper.ready"))
            .env("DCL30_FD", duplicate.to_string());
        let helper = helper.spawn().expect("spawning the lease helper");
        std::fs::write(fixture.join("helper-child.pid"), helper.id().to_string()).unwrap();
        std::fs::write(
            fixture.join("owner-ready.json"),
            serde_json::to_vec(&json!({"task_id":id,"directory":directory})).unwrap(),
        )
        .unwrap();
        // Stay alive as the real owner until the parent kills this process. The
        // helper is deliberately not waited for: it must outlive this process.
        tokio::time::sleep(Duration::from_secs(600)).await;
        drop(helper);
    }

    /// One owned fixture. Constructed (and cleaned up on every exit path) before
    /// any readiness wait, so a failing setup cannot leak the processes it
    /// already started.
    struct LinuxOwnerDeath {
        data: tempfile::TempDir,
        fixture: PathBuf,
        session: String,
        owner: Option<std::process::Child>,
        task_id: String,
        directory: PathBuf,
    }

    impl LinuxOwnerDeath {
        fn spawn(test_name: &str) -> Self {
            crate::config::OrchestrationConfig::init_global().unwrap();
            let data = tempfile::tempdir().unwrap();
            let fixture = data.path().join("probe");
            std::fs::create_dir_all(&fixture).unwrap();
            let session = uuid::Uuid::new_v4().to_string();
            let stdout = std::fs::File::create(fixture.join("owner.stdout.log")).unwrap();
            let stderr = std::fs::File::create(fixture.join("owner.stderr.log")).unwrap();
            let owner = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact")
                .arg(test_name)
                .arg("--nocapture")
                .env("DCL30_ROLE", "owner")
                .env("DCL30_ROOT", data.path())
                .env("DCL30_SESSION", &session)
                .env("DCL30_FIXTURE", &fixture)
                .stdout(stdout)
                .stderr(stderr)
                .spawn()
                .expect("spawning the owner process");
            Self {
                data,
                fixture,
                session,
                owner: Some(owner),
                task_id: String::new(),
                directory: PathBuf::new(),
            }
        }

        async fn ready(mut self) -> Self {
            let ready = self.fixture.join("owner-ready.json");
            wait_for_file(&ready, "owner readiness").await;
            let value: Value =
                serde_json::from_slice(&std::fs::read(&ready).unwrap()).expect("owner ready JSON");
            self.task_id = value["task_id"].as_str().unwrap().to_owned();
            self.directory = PathBuf::from(value["directory"].as_str().unwrap());
            wait_for_file(&self.fixture.join("shell.pid"), "pipe shell PID").await;
            wait_for_file(&self.fixture.join("helper.pid"), "lease helper PID").await;
            wait_for_file(&self.fixture.join("helper.ready"), "lease helper readiness").await;
            // Captured output must exist before the owner dies, so reading it
            // afterwards proves the log survived.
            wait_for_log_contains(
                &self.directory.join("stdout.log"),
                "ready",
                "pipe shell captured output",
            )
            .await;
            self
        }

        fn kill_owner(&mut self) {
            let mut owner = self.owner.take().expect("the owner is still running");
            owner.kill().expect("killing the owner process");
            owner.wait().expect("reaping the owner process");
        }

        async fn kill_helper(&self) {
            let pid = read_pid(&self.fixture.join("helper-child.pid"))
                .expect("the helper PID was published");
            kill_exact(pid);
            wait_process_finished(pid, "killed lease helper", &"the fixture").await;
        }

        fn raw_lease_held(&self) -> bool {
            let lease = std::fs::File::open(self.directory.join("owner.lock")).unwrap();
            super::lease_held(&lease).unwrap()
        }

        fn probe_state(&self) -> super::OwnerState {
            super::OwnerProbe::open(&self.directory)
                .expect("opening the owner probe")
                .state()
        }

        fn input(&self, data: &str) -> Result<Value> {
            super::super::shell_input::execute(
                &json!({"task_id":self.task_id,"data":data}),
                self.data.path(),
                &self.session,
            )
        }

        fn snapshot(&self) -> Value {
            super::super::tasks::execute_for_session(
                "TaskGet",
                json!({"task_id":self.task_id,"block":false}),
                self.data.path(),
                &self.session,
            )
            .expect("TaskGet snapshot")
        }
    }

    impl Drop for LinuxOwnerDeath {
        fn drop(&mut self) {
            if let Some(mut owner) = self.owner.take() {
                let _ = owner.kill();
                let _ = owner.wait();
            }
            // The shell and helper are not children of this process. Wait briefly
            // for the PID files a panic during setup may have outpaced, then kill
            // exactly those recorded PIDs. No pattern matching and no broad pkill.
            let deadline = std::time::Instant::now() + Duration::from_secs(2);
            loop {
                let pids: Vec<u32> = ["shell.pid", "helper-child.pid"]
                    .into_iter()
                    .filter_map(|name| read_pid(&self.fixture.join(name)))
                    .collect();
                if pids.len() == 2 || std::time::Instant::now() >= deadline {
                    for pid in pids {
                        kill_exact(pid);
                    }
                    break;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }

    /// The round-29 counterexample, now expected to be refused: a killed owner
    /// whose lease is still held by an inherited descriptor must not authorize
    /// input. The raw lock is still held, but the recorded original process is
    /// demonstrably gone, so the probe reports `Gone`.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn linux_owner_sigkill_with_inherited_lease_refuses_input() {
        const TEST: &str =
            "tools::background_shell::tests::linux_owner_sigkill_with_inherited_lease_refuses_input";
        if std::env::var("DCL30_ROLE").is_ok() {
            linux_owner_role().await;
            return;
        }
        let mut task = LinuxOwnerDeath::spawn(TEST).ready().await;
        task.kill_owner();
        // The fixture really reproduces the false-positive shape: another process
        // still holds the open file description, so the raw lock is still held.
        assert!(
            task.raw_lease_held(),
            "the inherited descriptor must keep the raw lease held"
        );
        let state = task.probe_state();
        println!("linux owner-death probe state: {state:?}");
        assert!(
            matches!(state, super::OwnerState::Gone(_)),
            "a killed owner still passed as live: {state:?}"
        );
        let result = task.input("orphan-probe\n");
        println!("linux owner-death TaskInput: {result:?}");
        let error = result.expect_err("a killed owner must refuse the write");
        let message = format!("{error:#}");
        assert!(
            message.contains("has exited") || message.contains("original owner is gone"),
            "{message}"
        );
        let log = task.snapshot();
        println!(
            "linux owner-death TaskGet status={:?} stdout={:?}",
            log["status"], log["stdout"]
        );
        assert_eq!(log["status"], "unresponsive", "{log}");
        assert!(
            log["stdout"].as_str().unwrap_or_default().contains("ready"),
            "the captured log was lost: {log}"
        );
    }

    /// Control for the test above: once the last sharer of the description is
    /// gone the lease is released, and the same call is refused with no owner at
    /// all. The log stays readable in both cases.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn linux_owner_last_shared_descriptor_closed_refuses_input() {
        const TEST: &str = "tools::background_shell::tests::linux_owner_last_shared_descriptor_closed_refuses_input";
        if std::env::var("DCL30_ROLE").is_ok() {
            linux_owner_role().await;
            return;
        }
        let mut task = LinuxOwnerDeath::spawn(TEST).ready().await;
        task.kill_owner();
        task.kill_helper().await;
        assert!(
            !task.raw_lease_held(),
            "no descriptor should hold the lease any more"
        );
        let state = task.probe_state();
        assert!(matches!(state, super::OwnerState::Gone(_)), "{state:?}");
        let error = task
            .input("orphan-probe\n")
            .expect_err("a released lease must refuse the write");
        assert!(
            format!("{error:#}").contains("owner lease is released"),
            "{error:#}"
        );
        let log = task.snapshot();
        assert!(
            log["stdout"].as_str().unwrap_or_default().contains("ready"),
            "the captured log was lost: {log}"
        );
    }

    /// A directory whose lease is genuinely held, with caller-chosen contents.
    /// The kept handle keeps the exclusive lock for as long as it lives.
    #[cfg(target_os = "linux")]
    fn held_lease(contents: &str) -> (tempfile::TempDir, std::fs::File) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("owner.lock");
        std::fs::write(&path, contents).unwrap();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap();
        (directory, file)
    }

    /// The identity is the point of the check: a live process with the recorded
    /// PID but a different creation time is a reused PID, not the owner.
    #[cfg(target_os = "linux")]
    #[test]
    fn identity_from_a_different_creation_of_the_same_pid_is_not_an_owner() {
        let pid = std::process::id();
        let start = super::process_start_time(pid).unwrap();
        let boot = super::read_boot_id().unwrap();
        let (directory, _held) = held_lease(&format!("dcl-owner-v1 {pid} {boot} {}\n", start + 1));
        let state = super::OwnerProbe::open(directory.path()).unwrap().state();
        assert!(
            matches!(state, super::OwnerState::Gone(_)),
            "a reused PID was accepted as the owner: {state:?}"
        );
        assert!(!state.is_held());
    }

    /// An identity recorded in another boot cannot match even when the PID and
    /// start time happen to be equal.
    #[cfg(target_os = "linux")]
    #[test]
    fn identity_from_another_boot_is_not_an_owner() {
        let pid = std::process::id();
        let start = super::process_start_time(pid).unwrap();
        let (directory, _held) = held_lease(&format!(
            "dcl-owner-v1 {pid} 00000000-0000-0000-0000-000000000000 {start}\n"
        ));
        let state = super::OwnerProbe::open(directory.path()).unwrap().state();
        assert!(
            matches!(state, super::OwnerState::Gone(_)),
            "an identity from another boot was accepted: {state:?}"
        );
        assert!(!state.is_held());
    }

    /// The identity of this very process is accepted, so the check is not simply
    /// refusing everything.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_current_process_identity_is_accepted_while_it_lives() {
        let identity = super::OwnerIdentity::current().unwrap();
        let (directory, _held) = held_lease(&identity.encode());
        let probe = super::OwnerProbe::open(directory.path()).unwrap();
        assert_eq!(probe.state(), super::OwnerState::Held);
        assert!(probe.state().is_held());
    }

    /// A held lease without a usable identity refuses input, while TaskGet keeps
    /// reading the output that was already captured.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn held_lease_without_identity_refuses_input_but_logs_stay_readable() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let task = super::super::tasks::execute_for_shell_owner(
            "TaskCreate",
            json!({"subject":"no identity","metadata":{"managed_shell":true,"interaction":"pipe"}}),
            root.path(),
            &session,
        )
        .unwrap();
        let id = task["id"].as_str().unwrap().to_owned();
        super::super::tasks::execute_for_shell_owner(
            "TaskUpdate",
            json!({"id":id,"status":"running"}),
            root.path(),
            &session,
        )
        .unwrap();
        let directory = root.path().join("shell-tasks").join(&session).join(&id);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("heartbeat"), b"running").unwrap();
        std::fs::write(directory.join("stdout.log"), b"captured output\n").unwrap();
        // Empty and malformed identities are the two shapes a pre-identity or
        // damaged record can take. Both are held leases, so the lock alone would
        // have authorized a write before this change.
        let forged = format!(
            "dcl-owner-v1 {} {} {}\n",
            std::process::id(),
            super::read_boot_id().unwrap(),
            super::process_start_time(std::process::id()).unwrap()
        );
        let extra_line = format!("{forged}trailing content\n");
        let out_of_range = format!(
            "dcl-owner-v1 {} {} {}\n",
            super::PID_MAX_LIMIT + 1,
            super::read_boot_id().unwrap(),
            super::process_start_time(std::process::id()).unwrap()
        );
        let zero_pid = format!("dcl-owner-v1 0 {} 1\n", super::read_boot_id().unwrap());
        for contents in [
            "",
            "not an owner identity\n",
            "dcl-owner-v1 12\n",
            extra_line.as_str(),
            out_of_range.as_str(),
            zero_pid.as_str(),
        ] {
            std::fs::write(directory.join("owner.lock"), contents).unwrap();
            let held = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(directory.join("owner.lock"))
                .unwrap();
            rustix::fs::flock(&held, rustix::fs::FlockOperation::NonBlockingLockExclusive).unwrap();
            let state = super::OwnerProbe::open(&directory).unwrap().state();
            assert!(
                matches!(state, super::OwnerState::Unreadable(_)),
                "a held lease without identity reported {state:?} for {contents:?}"
            );
            assert!(!state.is_held());
            let error = super::super::shell_input::execute(
                &json!({"task_id":id,"data":"must-not-be-written"}),
                root.path(),
                &session,
            )
            .expect_err("an unverified identity must refuse the write");
            assert!(
                format!("{error:#}").contains("no process identity"),
                "{error:#}"
            );
            let log = super::super::tasks::execute_for_session(
                "TaskGet",
                json!({"task_id":id,"block":false}),
                root.path(),
                &session,
            )
            .unwrap();
            assert_eq!(log["stdout"], "captured output\n", "{log}");
            crate::fs::unlock_file(&held).unwrap();
        }
    }

    /// A write that is already waiting for pipe capacity stops as soon as the
    /// owner dies, even while another descriptor keeps the lease held. It reports
    /// the bytes the OS accepted (none here) instead of a timeout, and nothing is
    /// retried.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn backpressure_wait_stops_when_the_owner_dies_with_a_shared_descriptor() {
        const TEST: &str = "tools::background_shell::tests::backpressure_wait_stops_when_the_owner_dies_with_a_shared_descriptor";
        if std::env::var("DCL30_ROLE").is_ok() {
            linux_owner_role().await;
            return;
        }
        let mut task = LinuxOwnerDeath::spawn(TEST).ready().await;
        // Fill the FIFO without going through the product: this is fixture setup.
        // The shell never reads it, so capacity is all that matters.
        {
            use std::io::Write;
            let fd = rustix::fs::open(
                &task.directory.join("stdin.pipe"),
                rustix::fs::OFlags::WRONLY
                    | rustix::fs::OFlags::NONBLOCK
                    | rustix::fs::OFlags::CLOEXEC,
                rustix::fs::Mode::empty(),
            )
            .unwrap();
            let mut filler = std::fs::File::from(fd);
            let chunk = vec![b'x'; 4096];
            loop {
                match filler.write(&chunk) {
                    Ok(0) => break,
                    Ok(_) => continue,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(error) => panic!("filling the pipe: {error}"),
                }
            }
        }
        // The production call must really be inside the capacity wait before the
        // owner dies, otherwise the initial check would be what refuses and the
        // cached-descriptor path would never be exercised. A one-shot latch in
        // the write's own `WouldBlock` arm supplies that proof; it blocks the
        // writer until this test resumes it, so there is no timing guess.
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel::<()>();
        let root = task.data.path().to_owned();
        let session = task.session.clone();
        let id = task.task_id.clone();
        let hook_id = id.clone();
        let writer = std::thread::spawn(move || {
            let mut fired = false;
            super::super::shell_input::capacity_barrier::install(Box::new(move |task| {
                if fired || task != hook_id {
                    return;
                }
                fired = true;
                let _ = entered_tx.send(());
                let _ = resume_rx.recv();
            }));
            let result = super::super::shell_input::execute(
                &json!({"task_id":id,"data":"blocked-payload","timeout":5000}),
                &root,
                &session,
            );
            super::super::shell_input::capacity_barrier::clear();
            result
        });
        // Bounded: a call that never blocks fails the test instead of hanging.
        entered_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("the write never reached the pipe-capacity wait");
        // The writer is suspended inside that wait; the owner dies while it holds
        // no lock on the probe it already cached.
        task.kill_owner();
        assert!(
            task.raw_lease_held(),
            "the helper must still hold the lease after the owner was killed"
        );
        assert!(
            matches!(task.probe_state(), super::OwnerState::Gone(_)),
            "the owner is not reported gone after the kill"
        );
        resume_tx.send(()).expect("the writer is still waiting");
        let killed = std::time::Instant::now();
        let result = writer.join().expect("the writer thread did not panic");
        let elapsed = killed.elapsed();
        println!("backpressure owner-death result after {elapsed:?}: {result:?}");
        let value = result.expect("the write must report partial progress, not fail");
        assert!(
            elapsed < Duration::from_secs(2),
            "the resumed call kept waiting after the owner died: {elapsed:?}"
        );
        assert_eq!(value["timed_out"], false, "{value}");
        assert_eq!(value["is_error"], true, "{value}");
        assert_eq!(value["owner_lost"], true, "{value}");
        assert_eq!(value["write_complete"], false, "{value}");
        assert_eq!(value["bytes_written"], 0, "{value}");
        // This wording comes from polling the pinned descriptor. A fresh
        // `/proc`-based identity check after the kill would instead report a
        // mismatch, so seeing it proves the cached pidfd was re-polled.
        assert!(
            value["error"]
                .as_str()
                .unwrap_or_default()
                .contains("has exited"),
            "the wait did not observe the pinned owner's exit: {value}"
        );
        let log = task.snapshot();
        assert!(
            log["stdout"].as_str().unwrap_or_default().contains("ready"),
            "the captured log was lost: {log}"
        );
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
                super::Interaction::None,
            )
            .unwrap()
        })
        .await;
        let id = task["task_id"].as_str().unwrap().to_owned();
        let finished = wait_until(root.path(), &session, &id, "completion", |value| {
            terminal(&value["status"])
        })
        .await;
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
                super::Interaction::None,
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

    fn paged_fixture(
        root: &Path,
        session: &str,
        stdout: &[u8],
        stderr: &[u8],
        status: &str,
    ) -> String {
        let task = super::super::tasks::execute_for_shell_owner(
            "TaskCreate",
            json!({"subject":"paged fixture","metadata":{"managed_shell":true}}),
            root,
            session,
        )
        .unwrap();
        let id = task["id"].as_str().unwrap().to_owned();
        super::super::tasks::execute_for_shell_owner(
            "TaskUpdate",
            json!({"id":id,"status":status}),
            root,
            session,
        )
        .unwrap();
        let directory = root.join("shell-tasks").join(session).join(&id);
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("heartbeat"), b"alive").unwrap();
        std::fs::write(directory.join("stdout.log"), stdout).unwrap();
        std::fs::write(directory.join("stderr.log"), stderr).unwrap();
        id
    }

    fn page_get(root: &Path, session: &str, input: Value) -> Value {
        super::super::tasks::execute_for_session("TaskGet", input, root, session).unwrap()
    }

    #[test]
    fn task_get_pages_are_bounded_and_concatenate_exactly() {
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let id = paged_fixture(
            root.path(),
            &session,
            b"alpha\nbeta\n",
            b"err-one\n",
            "running",
        );
        let first = page_get(
            root.path(),
            &session,
            json!({"task_id":id,"max_output_bytes":6}),
        );
        assert_eq!(first["output_mode"], "page");
        assert_eq!(first["stdout"], "alpha\n");
        assert_eq!(first["output_truncated"], true);
        assert_eq!(first["has_more"], true);
        // Page mode exposes only the page: no duplicate full text survives.
        assert_eq!(first["output"], Value::Null);
        assert_eq!(first["result"], Value::Null);

        let mut cursor = first["next_cursor"].as_str().unwrap().to_owned();
        let mut stdout = first["stdout"].as_str().unwrap().to_owned();
        let mut stderr = first["stderr"].as_str().unwrap().to_owned();
        for _ in 0..8 {
            let page = page_get(
                root.path(),
                &session,
                json!({"task_id":id,"cursor":cursor,"max_output_bytes":6}),
            );
            // The bound covers all returned log text, not each stream.
            assert!(
                page["stdout"].as_str().unwrap().len() + page["stderr"].as_str().unwrap().len()
                    <= 6,
                "{page}"
            );
            stdout.push_str(page["stdout"].as_str().unwrap());
            stderr.push_str(page["stderr"].as_str().unwrap());
            if page["has_more"] != true {
                assert_eq!(page["output_truncated"], false);
                break;
            }
            cursor = page["next_cursor"].as_str().unwrap().to_owned();
        }
        assert_eq!(stdout, "alpha\nbeta\n");
        assert_eq!(stderr, "err-one\n");
    }

    #[test]
    fn task_get_pages_never_split_a_utf8_character() {
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let id = paged_fixture(root.path(), &session, "aé€😀z".as_bytes(), b"", "running");
        let mut rebuilt = String::new();
        let mut cursor: Option<String> = None;
        for _ in 0..16 {
            let mut input = json!({"task_id":id,"max_output_bytes":4});
            if let Some(cursor) = &cursor {
                input["cursor"] = json!(cursor);
            }
            let page = page_get(root.path(), &session, input);
            rebuilt.push_str(page["stdout"].as_str().unwrap());
            if page["has_more"] != true {
                break;
            }
            cursor = Some(page["next_cursor"].as_str().unwrap().to_owned());
        }
        assert_eq!(rebuilt, "aé€😀z");
    }

    #[test]
    fn task_get_page_requests_validate_cursor_task_and_limit() {
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let id = paged_fixture(root.path(), &session, b"0123456789", b"", "running");
        let first = page_get(
            root.path(),
            &session,
            json!({"task_id":id,"max_output_bytes":4}),
        );
        let cursor = first["next_cursor"].as_str().unwrap().to_owned();
        // Replaying one cursor is deterministic and independent of other readers.
        let a = page_get(
            root.path(),
            &session,
            json!({"task_id":id,"cursor":cursor,"max_output_bytes":4}),
        );
        let b = page_get(
            root.path(),
            &session,
            json!({"task_id":id,"cursor":cursor,"max_output_bytes":4}),
        );
        assert_eq!(a["stdout"], b["stdout"]);
        assert_eq!(a["next_cursor"], b["next_cursor"]);

        // A cursor issued for another task never reads this task's output.
        let other = paged_fixture(root.path(), &session, b"other", b"", "running");
        for (label, input) in [
            ("foreign task", json!({"task_id":other,"cursor":cursor})),
            ("malformed", json!({"task_id":id,"cursor":"nonsense"})),
            ("zero bound", json!({"task_id":id,"max_output_bytes":0})),
            ("tiny bound", json!({"task_id":id,"max_output_bytes":3})),
            ("float bound", json!({"task_id":id,"max_output_bytes":4.5})),
        ] {
            assert!(
                super::super::tasks::execute_for_session("TaskGet", input, root.path(), &session)
                    .is_err(),
                "{label} must be rejected"
            );
        }
    }

    #[test]
    fn task_get_page_rejects_a_stale_cursor_after_truncation_or_rotation() {
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let id = paged_fixture(root.path(), &session, b"0123456789", b"", "running");
        let first = page_get(
            root.path(),
            &session,
            json!({"task_id":id,"max_output_bytes":4}),
        );
        let cursor = first["next_cursor"].as_str().unwrap().to_owned();
        let directory = root.path().join("shell-tasks").join(&session).join(&id);

        // A log shorter than the cursor position is stale, not an empty page.
        std::fs::write(directory.join("stdout.log"), b"01").unwrap();
        let error = super::super::tasks::execute_for_session(
            "TaskGet",
            json!({"task_id":id,"cursor":cursor}),
            root.path(),
            &session,
        )
        .unwrap_err();
        assert!(
            format!("{error}").to_lowercase().contains("stale"),
            "{error}"
        );

        // Rotation to a new inode is stale even when the new file is longer.
        let replacement = directory.join("stdout.new");
        std::fs::write(&replacement, b"0123456789-and-more").unwrap();
        std::fs::rename(&replacement, directory.join("stdout.log")).unwrap();
        let error = super::super::tasks::execute_for_session(
            "TaskGet",
            json!({"task_id":id,"cursor":cursor}),
            root.path(),
            &session,
        )
        .unwrap_err();
        assert!(
            format!("{error}").to_lowercase().contains("stale"),
            "{error}"
        );
    }

    #[test]
    fn task_get_pages_completed_and_empty_output() {
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let empty = paged_fixture(root.path(), &session, b"", b"", "completed");
        let page = page_get(
            root.path(),
            &session,
            json!({"task_id":empty,"max_output_bytes":16}),
        );
        assert_eq!(page["stdout"], "");
        assert_eq!(page["stderr"], "");
        assert_eq!(page["has_more"], false);
        assert_eq!(page["output_truncated"], false);
        assert_eq!(page["retrieval_status"], "success");
        assert!(page["next_cursor"].as_str().is_some_and(|v| !v.is_empty()));

        let done = paged_fixture(root.path(), &session, b"done-marker", b"", "completed");
        let mut cursor: Option<String> = None;
        let mut text = String::new();
        for _ in 0..8 {
            let mut input = json!({"task_id":done,"max_output_bytes":4});
            if let Some(cursor) = &cursor {
                input["cursor"] = json!(cursor);
            }
            let page = page_get(root.path(), &session, input);
            assert_eq!(page["retrieval_status"], "success");
            text.push_str(page["stdout"].as_str().unwrap());
            if page["has_more"] != true {
                break;
            }
            cursor = Some(page["next_cursor"].as_str().unwrap().to_owned());
        }
        assert_eq!(text, "done-marker");
    }

    #[test]
    fn task_get_without_page_fields_keeps_the_full_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let id = paged_fixture(root.path(), &session, b"full output", b"", "running");
        let value = page_get(root.path(), &session, json!({"task_id":id}));
        assert_eq!(value["stdout"], "full output");
        assert!(value.get("output_mode").is_none());
        assert!(value.get("next_cursor").is_none());
        assert!(value.get("max_output_bytes").is_none());
    }
}
