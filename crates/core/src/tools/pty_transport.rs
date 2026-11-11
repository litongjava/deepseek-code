//! Internal Linux pseudo-terminal transport for managed interactive shells.
//!
//! This is a foundation, not a user-visible interaction mode: nothing in the
//! tool registry or `TaskInput` selects it yet, it owns no task record or IPC
//! endpoint, and it does not read or write the durable task logs. It supplies
//! the parts a later managed-PTY owner needs:
//!
//! * a typed spawn spec (executable, exact argv, cwd, complete explicit
//!   environment, validated initial size);
//! * a real controlling terminal, because portable-pty starts the child with
//!   `setsid()` and `TIOCSCTTY(0)`;
//! * nonblocking master I/O. `O_NONBLOCK` belongs to the open file description,
//!   so the reader and writer duplicated from the master share it; both paths
//!   poll and treat `EAGAIN` as "not ready", never as a blocking read/write. A
//!   backend that cannot expose its master descriptor is refused instead of
//!   silently falling back to blocking calls;
//! * an absolute deadline and a cancellation flag per operation, with the bytes
//!   the terminal actually accepted reported on timeout or cancellation;
//! * resize through the backend, and VEOF injected only when the live termios
//!   says canonical mode and has a real VEOF character. Dropping the writer is
//!   never used as EOF: portable-pty's writer would inject bytes on drop;
//! * an RAII guard that kills and reaps exactly the process group this
//!   transport started.
//!
//! Termination guarantee: portable-pty gives the child its own session with
//! `setsid()`, so the child is both session leader and process-group leader
//! (`pgid == pid`). [`PtyTransport::stop`] and `Drop` signal that one group.
//! Processes that move to another process group (a job-control foreground job)
//! or that are reparented elsewhere are **not** covered. That is why `TaskStop`
//! integration is a later step and this module never claims a full descendant
//! tree.

use anyhow::{anyhow, bail, ensure, Context, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, ExitStatus, MasterPty, PtySize};
use rustix::fd::OwnedFd;
use std::{
    ffi::{OsStr, OsString},
    os::{fd::BorrowedFd, unix::ffi::OsStrExt},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};

/// Bytes one call may write. The same bound `TaskInput` already uses.
pub(crate) const MAX_INPUT_BYTES: usize = 64 * 1024;
/// Largest single read, so no caller can ask for an unbounded buffer.
pub(crate) const MAX_READ_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_ROWS: u16 = 500;
pub(crate) const MAX_COLS: u16 = 1000;
/// Longest single `poll` wait before the deadline and cancellation are rechecked.
const POLL_SLICE: Duration = Duration::from_millis(50);
/// How often a writer retries the shared write gate while another call holds it.
const GATE_SLICE: Duration = Duration::from_millis(5);
/// One `write(2)` at a time. A short write is progress, not an error.
const WRITE_CHUNK: usize = 8 * 1024;
/// Bound on reaping after `SIGKILL`, so `Drop` cannot wait forever.
const STOP_BOUND: Duration = Duration::from_secs(5);

/// Everything the child starts with.
///
/// The environment is the complete environment the transport asks for:
/// `CommandBuilder::env_clear()` runs before it is applied, so no inherited
/// variable leaks in. portable-pty's own spawn path then adds `SHELL` (its
/// documented behaviour); that is the only variable this module does not
/// control. `std::process::Command` cannot be used for this because it never
/// reveals which variables it inherited after an `env_clear`.
pub(crate) struct SpawnSpec {
    pub executable: OsString,
    pub arguments: Vec<OsString>,
    pub cwd: PathBuf,
    pub environment: Vec<(OsString, OsString)>,
    pub rows: u16,
    pub cols: u16,
}

/// The outcome of one bounded write. `accepted` counts bytes the terminal took,
/// not bytes a process consumed; on a timeout or cancellation the remainder is
/// reported and never retried implicitly.
#[derive(Debug)]
pub(crate) struct WriteOutcome {
    pub accepted: usize,
    pub remaining: usize,
    pub timed_out: bool,
    pub cancelled: bool,
    /// The terminal side is gone (EIO/EPIPE or a zero-byte write).
    pub closed: bool,
}

pub(crate) struct WriteRequest<'a> {
    pub data: &'a [u8],
    pub deadline: Instant,
    pub cancelled: Option<&'a AtomicBool>,
    /// Called after each chunk the terminal accepted. This is how a caller can
    /// watch partial progress while the call still runs; the tests use it to
    /// cancel deterministically instead of sleeping.
    pub on_accepted: Option<&'a mut dyn FnMut(usize)>,
}

#[derive(Debug)]
pub(crate) struct ReadOutcome {
    pub bytes: Vec<u8>,
    pub eof: bool,
    pub timed_out: bool,
    pub cancelled: bool,
}

#[derive(Debug)]
pub(crate) struct ResizeOutcome {
    pub rows: u16,
    pub cols: u16,
}

#[derive(Debug)]
pub(crate) struct EofOutcome {
    pub byte: u8,
}

/// Why VEOF could not be sent. A capability error leaves the terminal open and
/// unchanged; the caller must not guess a control byte.
#[derive(Debug)]
pub(crate) enum EofError {
    NoTermios,
    NotCanonical,
    VeofDisabled,
    Write(anyhow::Error),
}

impl std::fmt::Display for EofError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoTermios => write!(
                formatter,
                "the PTY backend exposes no termios for this terminal, so VEOF cannot be determined; no control byte was guessed and the terminal is unchanged"
            ),
            Self::NotCanonical => write!(
                formatter,
                "the terminal is not in canonical mode, so VEOF would be delivered as ordinary input; no EOF was sent and the terminal is unchanged"
            ),
            Self::VeofDisabled => write!(
                formatter,
                "the terminal has VEOF disabled, so there is no end-of-file character to send; the terminal is unchanged"
            ),
            Self::Write(error) => write!(formatter, "{error:#}"),
        }
    }
}

impl std::error::Error for EofError {}

impl From<anyhow::Error> for EofError {
    fn from(error: anyhow::Error) -> Self {
        Self::Write(error)
    }
}

/// The direct PTY child and the process group it created.
///
/// The group is the child's own PID because portable-pty calls `setsid()` in
/// the child. Nothing else is signalled: no current foreground group, no broad
/// PID range. `Drop` cleans up only while [`Self::armed`], which an explicit
/// [`Self::stop`] clears.
struct OwnedChild {
    child: Box<dyn Child + Send + Sync>,
    group: u32,
    finished: Option<ExitStatus>,
    armed: bool,
}

impl OwnedChild {
    fn new(child: Box<dyn Child + Send + Sync>, group: u32) -> Self {
        Self {
            child,
            group,
            finished: None,
            armed: true,
        }
    }

    fn process_id(&self) -> u32 {
        self.group
    }

    /// Signal this child's original process group, then reap with a bound.
    ///
    /// The group is signalled *before* any `try_wait`: until the leader is
    /// reaped its PID still names this process group, so `-pid` cannot reach an
    /// unrelated group. This also reaches descendants that stay in the original
    /// group after the leader exits normally (for example a background child
    /// that ignores SIGHUP), which a "reap first, signal only if still alive"
    /// order would miss. `armed` is cleared only after the reap succeeds, so an
    /// error leaves cleanup enabled for `Drop` or a later explicit stop.
    /// Calling it again returns the cached status, so repeat stops are equal.
    fn stop(&mut self) -> Result<Option<ExitStatus>> {
        if let Some(status) = &self.finished {
            return Ok(Some(status.clone()));
        }
        if let Some(pid) = rustix::process::Pid::from_raw(self.group as i32) {
            match rustix::process::kill_process_group(pid, rustix::process::Signal::KILL) {
                // Signalled, or the group is already empty (for example only a
                // zombie leader was left): reaping is safe in both cases.
                Ok(()) | Err(rustix::io::Errno::SRCH) => {}
                // Any other failure means the original group was not signalled.
                // Do not reap or disarm here: reporting a successful cleanup
                // would be false, and Drop or a later stop must retry.
                Err(error) => {
                    return Err(error).context("Signalling the PTY child's original process group")
                }
            }
        }
        let deadline = Instant::now() + STOP_BOUND;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.finished = Some(status.clone());
                    self.armed = false;
                    return Ok(Some(status));
                }
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Ok(None) => bail!(
                    "PTY child {} did not exit within {STOP_BOUND:?} after SIGKILL",
                    self.group
                ),
                Err(error) => return Err(error).context("Reaping the PTY child"),
            }
        }
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.armed {
            if let Err(error) = self.stop() {
                tracing::warn!(%error, "PTY child cleanup failed");
            }
        }
    }
}

pub(crate) struct PtyTransport {
    // Declared first so cleanup runs before the terminal descriptors close.
    child: Mutex<OwnedChild>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    read_fd: OwnedFd,
    write_fd: OwnedFd,
    write_gate: Mutex<()>,
}

impl PtyTransport {
    pub(crate) fn process_id(&self) -> u32 {
        lock(&self.child).process_id()
    }

    /// Read at most `max` bytes, waiting no longer than `deadline`.
    ///
    /// Returns as soon as one read produced bytes (or EOF); the caller decides
    /// whether it needs more. Bytes are returned raw: the transport never
    /// decodes text, so a UTF-8 character split across reads stays split.
    pub(crate) fn read_bytes(
        &self,
        max: usize,
        deadline: Instant,
        cancelled: Option<&AtomicBool>,
    ) -> Result<ReadOutcome> {
        ensure!(
            (1..=MAX_READ_BYTES).contains(&max),
            "A PTY read must request 1..={MAX_READ_BYTES} bytes, not {max}"
        );
        loop {
            if is_cancelled(cancelled) {
                return Ok(ReadOutcome {
                    bytes: Vec::new(),
                    eof: false,
                    timed_out: false,
                    cancelled: true,
                });
            }
            match wait_for(
                &self.read_fd,
                rustix::event::PollFlags::IN,
                deadline,
                cancelled,
            )? {
                Waited::Cancelled => {
                    return Ok(ReadOutcome {
                        bytes: Vec::new(),
                        eof: false,
                        timed_out: false,
                        cancelled: true,
                    })
                }
                Waited::TimedOut => {
                    return Ok(ReadOutcome {
                        bytes: Vec::new(),
                        eof: false,
                        timed_out: true,
                        cancelled: false,
                    })
                }
                Waited::Ready => {}
            }
            let mut buffer = vec![0u8; max];
            match rustix::io::read(&self.read_fd, &mut buffer[..]) {
                Ok(0) => {
                    return Ok(ReadOutcome {
                        bytes: Vec::new(),
                        eof: true,
                        timed_out: false,
                        cancelled: false,
                    })
                }
                Ok(count) => {
                    buffer.truncate(count);
                    return Ok(ReadOutcome {
                        bytes: buffer,
                        eof: false,
                        timed_out: false,
                        cancelled: false,
                    });
                }
                Err(rustix::io::Errno::AGAIN) | Err(rustix::io::Errno::INTR) => continue,
                // The slave side is gone: Linux reports EIO on a master read.
                Err(rustix::io::Errno::IO) => {
                    return Ok(ReadOutcome {
                        bytes: Vec::new(),
                        eof: true,
                        timed_out: false,
                        cancelled: false,
                    })
                }
                Err(error) => return Err(error).context("Reading from the PTY master"),
            }
        }
    }

    /// Write with an absolute deadline and an optional cancellation flag.
    ///
    /// Bytes are serialized with the gate, and each chunk is counted only after
    /// the terminal accepted it. A timeout or cancellation returns the accepted
    /// prefix; nothing is retried after the outcome is reported.
    /// Acquire the per-terminal write gate without exceeding this call's own
    /// deadline. A second writer must not sit behind a long first writer and
    /// then report success long after its own timeout, so the gate is taken
    /// with `try_lock` in short slices and both the deadline and the
    /// cancellation flag are rechecked each slice.
    fn acquire_gate<'a>(&'a self, deadline: Instant, cancelled: Option<&AtomicBool>) -> Gate<'a> {
        loop {
            if is_cancelled(cancelled) {
                return Gate::Cancelled;
            }
            if Instant::now() >= deadline {
                return Gate::TimedOut;
            }
            match self.write_gate.try_lock() {
                Ok(guard) => return Gate::Acquired(guard),
                Err(std::sync::TryLockError::Poisoned(poisoned)) => {
                    return Gate::Acquired(poisoned.into_inner())
                }
                Err(std::sync::TryLockError::WouldBlock) => {
                    #[cfg(test)]
                    wait_probe::waiting();
                    std::thread::sleep(GATE_SLICE);
                }
            }
        }
    }

    pub(crate) fn write_bytes(&self, mut request: WriteRequest<'_>) -> Result<WriteOutcome> {
        ensure!(
            request.data.len() <= MAX_INPUT_BYTES,
            "One PTY write accepts at most {MAX_INPUT_BYTES} bytes, not {}",
            request.data.len()
        );
        let total = request.data.len();
        // Nothing is written while waiting for the gate, so a gate timeout or
        // cancellation reports zero accepted bytes and does not touch the
        // terminal at all.
        let _gate = match self.acquire_gate(request.deadline, request.cancelled) {
            Gate::Acquired(guard) => guard,
            Gate::TimedOut => {
                return Ok(WriteOutcome {
                    accepted: 0,
                    remaining: total,
                    timed_out: true,
                    cancelled: false,
                    closed: false,
                })
            }
            Gate::Cancelled => {
                return Ok(WriteOutcome {
                    accepted: 0,
                    remaining: total,
                    timed_out: false,
                    cancelled: true,
                    closed: false,
                })
            }
        };
        let mut accepted = 0usize;
        while accepted < total {
            if is_cancelled(request.cancelled) {
                return Ok(WriteOutcome {
                    accepted,
                    remaining: total - accepted,
                    timed_out: false,
                    cancelled: true,
                    closed: false,
                });
            }
            match wait_for(
                &self.write_fd,
                rustix::event::PollFlags::OUT,
                request.deadline,
                request.cancelled,
            )? {
                Waited::Cancelled => {
                    return Ok(WriteOutcome {
                        accepted,
                        remaining: total - accepted,
                        timed_out: false,
                        cancelled: true,
                        closed: false,
                    })
                }
                Waited::TimedOut => {
                    return Ok(WriteOutcome {
                        accepted,
                        remaining: total - accepted,
                        timed_out: true,
                        cancelled: false,
                        closed: false,
                    })
                }
                Waited::Ready => {}
            }
            let end = (accepted + WRITE_CHUNK).min(total);
            match rustix::io::write(&self.write_fd, &request.data[accepted..end]) {
                Ok(0) => {
                    return Ok(WriteOutcome {
                        accepted,
                        remaining: total - accepted,
                        timed_out: false,
                        cancelled: false,
                        closed: true,
                    })
                }
                Ok(count) => {
                    accepted += count;
                    if let Some(progress) = request.on_accepted.as_mut() {
                        progress(count);
                    }
                }
                Err(rustix::io::Errno::AGAIN) | Err(rustix::io::Errno::INTR) => continue,
                // EPIPE/EIO: the terminal side is gone. The caller still learns
                // exactly how much was accepted before that point.
                Err(rustix::io::Errno::PIPE) | Err(rustix::io::Errno::IO) => {
                    return Ok(WriteOutcome {
                        accepted,
                        remaining: total - accepted,
                        timed_out: false,
                        cancelled: false,
                        closed: true,
                    })
                }
                Err(error) => return Err(error).context("Writing to the PTY master"),
            }
        }
        Ok(WriteOutcome {
            accepted,
            remaining: 0,
            timed_out: false,
            cancelled: false,
            closed: false,
        })
    }

    pub(crate) fn size(&self) -> Result<(u16, u16)> {
        let master = lock(&self.master);
        let size = master.get_size().context("Reading the PTY size")?;
        Ok((size.rows, size.cols))
    }

    /// Validate first, mutate second, then report what the terminal now says.
    pub(crate) fn resize(&self, rows: u16, cols: u16) -> Result<ResizeOutcome> {
        validate_dimensions(rows, cols)?;
        let master = lock(&self.master);
        master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("Resizing the PTY")?;
        let size = master
            .get_size()
            .context("Reading the PTY size after resizing")?;
        Ok(ResizeOutcome {
            rows: size.rows,
            cols: size.cols,
        })
    }

    /// The end-of-file byte this terminal currently uses, if it can send one.
    ///
    /// This is a capability query: it never mutates the terminal. A caller must
    /// treat an error as "cannot send VEOF", not as permission to guess.
    pub(crate) fn veof_byte(&self) -> std::result::Result<u8, EofError> {
        let master = lock(&self.master);
        let termios = master.get_termios().ok_or(EofError::NoTermios)?;
        if !termios
            .local_flags
            .contains(nix::sys::termios::LocalFlags::ICANON)
        {
            return Err(EofError::NotCanonical);
        }
        let byte = termios.control_chars[nix::sys::termios::SpecialCharacterIndices::VEOF as usize];
        if byte == 0 || byte == nix::sys::termios::_POSIX_VDISABLE {
            return Err(EofError::VeofDisabled);
        }
        Ok(byte)
    }

    /// Send the terminal's own VEOF character once.
    ///
    /// This is not "close stdin": the descriptor stays open and a later write
    /// can still be accepted. Whatever the terminal does with VEOF is its line
    /// discipline's decision, not a permanent closure claimed by this module.
    pub(crate) fn send_eof(
        &self,
        deadline: Instant,
        cancelled: Option<&AtomicBool>,
    ) -> std::result::Result<EofOutcome, EofError> {
        let byte = self.veof_byte()?;
        let data = [byte];
        let outcome = self.write_bytes(WriteRequest {
            data: &data,
            deadline,
            cancelled,
            on_accepted: None,
        })?;
        if outcome.accepted != 1 {
            return Err(EofError::Write(anyhow!(
                "the terminal accepted {} of 1 VEOF bytes",
                outcome.accepted
            )));
        }
        Ok(EofOutcome { byte })
    }

    /// Terminate this transport's process group and return its exit status.
    pub(crate) fn stop(&self) -> Result<Option<ExitStatus>> {
        lock(&self.child).stop()
    }
}

/// Spawn a command on a new pseudo-terminal.
///
/// All validation happens before `openpty`, so an invalid request creates no
/// terminal and no process. Once a child exists, every later failure goes
/// through [`OwnedChild`]'s drop, which kills and reaps it before this function
/// returns the error.
pub(crate) fn spawn(spec: &SpawnSpec) -> Result<PtyTransport> {
    validate_spec(spec)?;
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: spec.rows,
            cols: spec.cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("Opening a pseudo-terminal")?;
    let mut builder = CommandBuilder::new(&spec.executable);
    builder.args(spec.arguments.iter());
    builder.cwd(&spec.cwd);
    // Clear the inherited environment and apply exactly what the caller asked
    // for. Nothing from the engine's own environment reaches the terminal.
    builder.env_clear();
    for (name, value) in &spec.environment {
        builder.env(name, value);
    }
    let child = match pair.slave.spawn_command(builder) {
        Ok(child) => child,
        Err(error) => bail!("Starting the PTY child: {error}"),
    };
    // The child owns its own duplicate of the slave for stdin/stdout/stderr;
    // a copy left open here would keep the terminal from reporting EOF.
    drop(pair.slave);
    let master = pair.master;
    let Some(group) = child.process_id() else {
        // Without a PID there is no group identity to own. Stop the direct
        // child and refuse rather than leave a terminal nobody can clean up.
        let mut child = child;
        let _ = child.kill();
        let _ = child.wait();
        bail!("The PTY child reported no process ID; refusing to own a terminal without a process identity");
    };
    let owned = OwnedChild::new(child, group);
    // From here on an error drops `owned`, which signals the group and reaps it.
    let raw = master.as_raw_fd().context(
        "The PTY backend did not expose its master descriptor; refusing to fall back to blocking terminal I/O",
    )?;
    // SAFETY: `raw` belongs to `master`, which outlives this borrow and is moved
    // into the returned transport.
    let borrowed = unsafe { BorrowedFd::borrow_raw(raw) };
    let flags = rustix::fs::fcntl_getfl(borrowed).context("Reading the PTY master flags")?;
    rustix::fs::fcntl_setfl(borrowed, flags | rustix::fs::OFlags::NONBLOCK)
        .context("Making the PTY master nonblocking")?;
    let read_fd = rustix::io::fcntl_dupfd_cloexec(borrowed, 0)
        .context("Duplicating the PTY master for reading")?;
    let write_fd = rustix::io::fcntl_dupfd_cloexec(borrowed, 0)
        .context("Duplicating the PTY master for writing")?;
    Ok(PtyTransport {
        child: Mutex::new(owned),
        master: Mutex::new(master),
        read_fd,
        write_fd,
        write_gate: Mutex::new(()),
    })
}

fn validate_spec(spec: &SpawnSpec) -> Result<()> {
    ensure!(!spec.executable.is_empty(), "A PTY needs an executable");
    ensure!(
        Path::new(&spec.cwd).is_dir(),
        "PTY working directory does not exist: {}",
        spec.cwd.display()
    );
    validate_dimensions(spec.rows, spec.cols)?;
    for (name, _) in &spec.environment {
        ensure!(
            !name.is_empty() && !name.as_bytes().contains(&b'='),
            "An environment variable name must be non-empty and cannot contain '='"
        );
    }
    Ok(())
}

fn validate_dimensions(rows: u16, cols: u16) -> Result<()> {
    ensure!(
        (1..=MAX_ROWS).contains(&rows),
        "PTY rows must be in 1..={MAX_ROWS}, not {rows}"
    );
    ensure!(
        (1..=MAX_COLS).contains(&cols),
        "PTY columns must be in 1..={MAX_COLS}, not {cols}"
    );
    Ok(())
}

fn is_cancelled(flag: Option<&AtomicBool>) -> bool {
    flag.is_some_and(|flag| flag.load(Ordering::Acquire))
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Why a wait ended.
enum Waited {
    Ready,
    TimedOut,
    Cancelled,
}

/// Result of trying to take the write gate within one call's own budget.
enum Gate<'a> {
    Acquired(std::sync::MutexGuard<'a, ()>),
    TimedOut,
    Cancelled,
}

/// Wait until `fd` is ready, the deadline passes, or cancellation is observed.
///
/// The loop is sliced so cancellation is noticed at most one `POLL_SLICE`
/// after it is set, even while the terminal never becomes ready. A ready
/// descriptor may still report HUP/ERR; the caller's read or write turns that
/// into EOF or EIO.
fn wait_for(
    fd: &OwnedFd,
    events: rustix::event::PollFlags,
    deadline: Instant,
    cancelled: Option<&AtomicBool>,
) -> Result<Waited> {
    loop {
        if is_cancelled(cancelled) {
            return Ok(Waited::Cancelled);
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(Waited::TimedOut);
        }
        let slice = deadline.saturating_duration_since(now).min(POLL_SLICE);
        let timeout = rustix::event::Timespec {
            tv_sec: slice.as_secs() as _,
            tv_nsec: slice.subsec_nanos() as _,
        };
        let mut fds = [rustix::event::PollFd::new(fd, events)];
        match rustix::event::poll(&mut fds, Some(&timeout)) {
            // This slice expired with no readiness. The loop rechecks
            // cancellation and the deadline before waiting again.
            Ok(0) => {
                #[cfg(test)]
                wait_probe::waiting();
                continue;
            }
            Ok(_) => return Ok(Waited::Ready),
            Err(rustix::io::Errno::INTR) => continue,
            Err(error) => return Err(error).context("Waiting for the PTY master"),
        }
    }
}

/// Test-only observation point for "the call is inside a real no-readiness
/// wait". It is thread-local, so it belongs to the one call that installed it,
/// and is compiled out of release builds. Tests use it to set a cancellation
/// flag at the exact moment the old implementation would still be sleeping
/// until the deadline.
#[cfg(test)]
mod wait_probe {
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

    pub(super) fn waiting() {
        HOOK.with(|slot| {
            if let Some(hook) = slot.borrow_mut().as_mut() {
                hook();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::ffi::OsStringExt;

    /// PTY tests own real processes and terminal descriptors; running them one
    /// at a time keeps process/fd observations meaningful.
    static SERIAL: Mutex<()> = Mutex::new(());

    const BUDGET: Duration = Duration::from_secs(10);

    fn run_serial() -> std::sync::MutexGuard<'static, ()> {
        SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn spec(script: &str) -> SpawnSpec {
        SpawnSpec {
            executable: OsString::from("/bin/sh"),
            arguments: vec![OsString::from("-c"), OsString::from(script)],
            cwd: std::env::temp_dir(),
            environment: vec![(OsString::from("PATH"), OsString::from("/usr/bin:/bin"))],
            rows: 24,
            cols: 80,
        }
    }

    fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
        !needle.is_empty()
            && haystack
                .windows(needle.len())
                .any(|window| window == needle)
    }

    fn read_until(transport: &PtyTransport, needle: &[u8], budget: Duration) -> Vec<u8> {
        let deadline = Instant::now() + budget;
        let mut collected = Vec::new();
        loop {
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {:?}; collected {:?}",
                    String::from_utf8_lossy(needle),
                    String::from_utf8_lossy(&collected)
                );
            }
            let outcome = transport.read_bytes(4096, deadline, None).unwrap();
            collected.extend_from_slice(&outcome.bytes);
            if contains_bytes(&collected, needle) {
                return collected;
            }
            if outcome.eof {
                panic!(
                    "EOF before {:?}; collected {:?}",
                    String::from_utf8_lossy(needle),
                    String::from_utf8_lossy(&collected)
                );
            }
        }
    }

    fn read_to_eof(transport: &PtyTransport, budget: Duration) -> Vec<u8> {
        let deadline = Instant::now() + budget;
        let mut collected = Vec::new();
        loop {
            if Instant::now() >= deadline {
                panic!(
                    "timed out reading to EOF; collected {:?}",
                    String::from_utf8_lossy(&collected)
                );
            }
            let outcome = transport.read_bytes(4096, deadline, None).unwrap();
            collected.extend_from_slice(&outcome.bytes);
            if outcome.eof {
                return collected;
            }
        }
    }

    fn write_all(transport: &PtyTransport, data: &[u8]) {
        let outcome = transport
            .write_bytes(WriteRequest {
                data,
                deadline: Instant::now() + Duration::from_secs(5),
                cancelled: None,
                on_accepted: None,
            })
            .unwrap();
        assert_eq!(outcome.accepted, data.len(), "{outcome:?}");
        assert!(!outcome.timed_out && !outcome.cancelled && !outcome.closed);
    }

    /// Sets an owned cancellation flag from inside the transport's no-readiness
    /// wait. Thread-local and cleared on drop, so a panic cannot leak it into
    /// another test that reuses this thread.
    struct WaitProbe;
    impl WaitProbe {
        fn install(cancelled: std::sync::Arc<AtomicBool>) -> Self {
            let mut fired = false;
            super::wait_probe::install(Box::new(move || {
                if fired {
                    return;
                }
                fired = true;
                cancelled.store(true, Ordering::Release);
            }));
            Self
        }
    }
    impl Drop for WaitProbe {
        fn drop(&mut self) {
            super::wait_probe::clear();
        }
    }

    const SENTINEL: &[u8] = b"SENTINEL-END\n";

    /// A child that reports READY, then waits for `gate` and finally copies its
    /// stdin to the terminal. A byte appears in the output only after the child
    /// consumed it, so a test can prove that exactly the accepted prefix was
    /// consumed and that nothing wrote more bytes afterwards.
    fn gated_reader() -> (tempfile::TempDir, PathBuf, PtyTransport) {
        let directory = tempfile::tempdir().unwrap();
        let gate = directory.path().join("gate");
        let transport = spawn(&spec(&format!(
            "stty raw -echo; printf 'READY\\n'; while [ ! -e '{}' ]; do sleep 0.01; done; cat",
            gate.display()
        )))
        .unwrap();
        read_until(&transport, b"READY", BUDGET);
        (directory, gate, transport)
    }

    /// Open the gate, send a sentinel, and return everything the child echoed
    /// back up to that sentinel. Because the child copies stdin to the terminal
    /// this is exactly what it consumed, so a late writer would show up here.
    fn consume_and_collect(
        transport: &PtyTransport,
        gate: &Path,
        expected_prefix: usize,
        prefix_byte: u8,
    ) -> Vec<u8> {
        std::fs::write(gate, b"open").unwrap();
        let outcome = transport
            .write_bytes(WriteRequest {
                data: SENTINEL,
                deadline: Instant::now() + Duration::from_secs(10),
                cancelled: None,
                on_accepted: None,
            })
            .unwrap();
        assert_eq!(outcome.accepted, SENTINEL.len(), "{outcome:?}");
        let output = read_until(transport, SENTINEL, BUDGET);
        let end = output
            .windows(SENTINEL.len())
            .position(|window| window == SENTINEL)
            .expect("the sentinel must be present")
            + SENTINEL.len();
        assert_eq!(
            end,
            output.len(),
            "bytes after the sentinel: {:?}",
            String::from_utf8_lossy(&output[end..])
        );
        assert_eq!(
            output.iter().filter(|byte| **byte == prefix_byte).count(),
            expected_prefix,
            "the child consumed a different prefix than the write accepted"
        );
        assert_eq!(
            output.len(),
            expected_prefix + SENTINEL.len(),
            "a later write added bytes"
        );
        output
    }

    fn process_group(pid: u32) -> Option<u32> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // After "pid (comm)" come state, ppid, pgrp.
        stat.rsplit_once(')')?
            .1
            .split_whitespace()
            .nth(2)?
            .parse()
            .ok()
    }

    /// A leader that exits normally, leaving a same-group background child that
    /// ignores SIGHUP. The direct child is not reaped before `stop`, so its PID
    /// still reserves the group; that is the exact case a "reap first, then
    /// signal only if still alive" cleanup would miss.
    fn exited_leader_with_survivor() -> (tempfile::TempDir, PtyTransport, u32, u32) {
        let directory = tempfile::tempdir().unwrap();
        let transport = spawn(&spec(
            "trap '' HUP; sleep 30 & printf 'GC=%s\\n' \"$!\"; printf 'READY\\n'; exit 0",
        ))
        .unwrap();
        let output = read_until(&transport, b"READY", BUDGET);
        let text = String::from_utf8_lossy(&output);
        let survivor: u32 = text
            .split("GC=")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| panic!("no survivor PID in {text:?}"));
        let leader = transport.process_id();
        assert_eq!(
            process_group(survivor),
            Some(leader),
            "the survivor is not in the leader's original process group"
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while process_alive(leader) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!process_alive(leader), "the leader never exited");
        assert!(
            process_alive(survivor),
            "the survivor did not outlive the leader"
        );
        (directory, transport, leader, survivor)
    }

    fn process_alive(pid: u32) -> bool {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => stat
                .rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().next())
                .is_some_and(|state| state != "Z"),
            Err(_) => false,
        }
    }

    fn wait_until_gone(pid: u32, budget: Duration) {
        let deadline = Instant::now() + budget;
        while process_alive(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!process_alive(pid), "process {pid} is still alive");
    }

    #[test]
    fn a_child_runs_on_a_controlling_terminal() {
        let _serial = run_serial();
        let transport = spawn(&spec(
            "[ -t 0 ] && [ -t 1 ] && [ -t 2 ] || exit 9; \
             tty; stty size; \
             if (: </dev/tty) 2>/dev/null; then printf 'CTTY-OK\\n'; else printf 'CTTY-MISSING\\n'; fi",
        ))
        .unwrap();
        let output = read_until(&transport, b"CTTY-OK", BUDGET);
        let text = String::from_utf8_lossy(&output);
        assert!(text.contains("/dev/pts/"), "{text}");
        assert!(text.contains("24 80"), "{text}");
        assert!(!text.contains("CTTY-MISSING"), "{text}");
        assert_eq!(transport.size().unwrap(), (24, 80));
        assert!(transport.stop().unwrap().is_some());
    }

    #[test]
    fn arguments_cwd_and_non_utf8_are_preserved_exactly() {
        let _serial = run_serial();
        let directory = tempfile::tempdir().unwrap();
        let canonical = std::fs::canonicalize(directory.path()).unwrap();
        let script = "printf 'ARG0=<%s>\\n' \"$0\"; printf 'ARG1=<%s>\\n' \"$1\"; \
                      printf 'CWD=<%s>\\n' \"$(pwd -P)\"; printf 'READY\\n'";
        let raw = OsString::from_vec(vec![b'a', b'r', b'g', b'-', 0xff, 0xfe]);
        let transport = spawn(&SpawnSpec {
            executable: OsString::from("/bin/sh"),
            arguments: vec![
                OsString::from("-c"),
                OsString::from(script),
                raw.clone(),
                OsString::from("one value"),
            ],
            cwd: canonical.clone(),
            environment: vec![(OsString::from("PATH"), OsString::from("/usr/bin:/bin"))],
            rows: 24,
            cols: 80,
        })
        .unwrap();
        let output = read_until(&transport, b"READY", BUDGET);
        assert!(
            contains_bytes(&output, &[b'a', b'r', b'g', b'-', 0xff, 0xfe]),
            "the non-UTF8 argument did not arrive byte-exactly: {output:?}"
        );
        assert!(
            String::from_utf8_lossy(&output).contains(&format!("CWD=<{}>", canonical.display())),
            "{}",
            String::from_utf8_lossy(&output)
        );
        assert!(transport.stop().unwrap().is_some());
    }

    #[test]
    fn the_child_environment_is_exactly_the_specification() {
        let _serial = run_serial();
        let transport = spawn(&SpawnSpec {
            executable: OsString::from("/usr/bin/env"),
            arguments: Vec::new(),
            cwd: std::env::temp_dir(),
            environment: vec![
                (OsString::from("PATH"), OsString::from("/usr/bin:/bin")),
                (OsString::from("DCL_PTY_TEST"), OsString::from("value")),
            ],
            rows: 24,
            cols: 80,
        })
        .unwrap();
        let output = read_to_eof(&transport, BUDGET);
        let text = String::from_utf8_lossy(&output);
        assert!(text.contains("DCL_PTY_TEST=value"), "{text}");
        assert!(
            !text.contains("HOME="),
            "an inherited HOME reached the child: {text}"
        );
        assert!(
            !text.contains("DEEPSEEK_CODE_"),
            "an inherited internal variable reached the child: {text}"
        );
        // portable-pty's spawn path always adds SHELL; documented exception.
        assert!(text.contains("SHELL="), "{text}");
        assert_eq!(transport.stop().unwrap().unwrap().exit_code(), 0);
    }

    /// The error text of a spawn that must be refused. Starting a transport
    /// here would mean validation ran too late.
    fn refused(spec: &SpawnSpec) -> String {
        match spawn(spec) {
            Ok(transport) => {
                let _ = transport.stop();
                panic!("an invalid spawn spec started a terminal");
            }
            Err(error) => error.to_string(),
        }
    }

    #[test]
    fn invalid_spawn_requests_are_refused_before_a_terminal_exists() {
        let _serial = run_serial();
        let mut bad_rows = spec("true");
        bad_rows.rows = 0;
        assert!(refused(&bad_rows).contains("rows"));

        let mut bad_cols = spec("true");
        bad_cols.cols = MAX_COLS + 1;
        assert!(refused(&bad_cols).contains("columns"));

        let mut bad_cwd = spec("true");
        bad_cwd.cwd = PathBuf::from("/definitely/not/a/pty/directory");
        assert!(refused(&bad_cwd).contains("directory"));

        let mut bad_executable = spec("true");
        bad_executable.executable = OsString::new();
        assert!(refused(&bad_executable).contains("executable"));

        let mut bad_environment = spec("true");
        bad_environment.environment = vec![(OsString::from("NOT=VALID"), OsString::from("1"))];
        assert!(refused(&bad_environment).contains("environment variable name"));

        // Validation runs before `openpty`, so no terminal or child was made.
        // A valid start still works afterwards, proving nothing is stuck.
        let transport = spawn(&spec("printf 'READY\\n'; sleep 30")).unwrap();
        read_until(&transport, b"READY", BUDGET);
        transport.stop().unwrap();
    }

    #[test]
    fn the_child_observes_a_native_resize() {
        let _serial = run_serial();
        let transport = spawn(&spec(
            "stty size; printf 'READY\\n'; read line; stty size; printf 'DONE\\n'",
        ))
        .unwrap();
        let output = read_until(&transport, b"READY", BUDGET);
        assert!(
            String::from_utf8_lossy(&output).contains("24 80"),
            "{}",
            String::from_utf8_lossy(&output)
        );
        let resized = transport.resize(40, 120).unwrap();
        assert_eq!((resized.rows, resized.cols), (40, 120));
        assert_eq!(transport.size().unwrap(), (40, 120));
        write_all(&transport, b"\n");
        let output = read_until(&transport, b"DONE", BUDGET);
        assert!(
            String::from_utf8_lossy(&output).contains("40 120"),
            "the child did not see the resize: {}",
            String::from_utf8_lossy(&output)
        );
        transport.stop().unwrap();
    }

    #[test]
    fn invalid_dimensions_are_refused_without_changing_the_terminal() {
        let _serial = run_serial();
        let transport = spawn(&spec("printf 'READY\\n'; sleep 30")).unwrap();
        read_until(&transport, b"READY", BUDGET);
        assert!(transport.resize(0, 80).is_err());
        assert!(transport.resize(MAX_ROWS + 1, 80).is_err());
        assert!(transport.resize(24, 0).is_err());
        assert!(transport.resize(24, MAX_COLS + 1).is_err());
        assert_eq!(transport.size().unwrap(), (24, 80));
        let resized = transport.resize(30, 100).unwrap();
        assert_eq!((resized.rows, resized.cols), (30, 100));
        assert_eq!(transport.size().unwrap(), (30, 100));
        transport.stop().unwrap();
    }

    #[test]
    fn unicode_input_is_echoed_and_veof_is_not_a_permanent_closure() {
        let _serial = run_serial();
        let transport = spawn(&spec("cat; printf 'DONE\\n'; sleep 30")).unwrap();
        let message = "héllo 🌍\n".as_bytes();
        write_all(&transport, message);
        let echoed = read_until(&transport, "héllo 🌍".as_bytes(), BUDGET);
        assert!(
            contains_bytes(&echoed, "héllo 🌍".as_bytes()),
            "{}",
            String::from_utf8_lossy(&echoed)
        );
        let expected = transport.veof_byte().unwrap();
        // A freshly opened pty uses the kernel's default canonical settings.
        assert_eq!(expected, 0x04, "unexpected default VEOF byte");
        let first = transport
            .send_eof(Instant::now() + Duration::from_secs(5), None)
            .unwrap();
        assert_eq!(first.byte, expected);
        let output = read_until(&transport, b"DONE", BUDGET);
        assert!(String::from_utf8_lossy(&output).contains("DONE"));
        // VEOF is input, not closure: another VEOF and ordinary bytes are still
        // accepted, and the transport never claims stdin is closed.
        let second = transport
            .send_eof(Instant::now() + Duration::from_secs(5), None)
            .unwrap();
        assert_eq!(second.byte, expected);
        write_all(&transport, b"later\n");
        assert!(transport.stop().unwrap().is_some());
    }

    #[test]
    fn raw_mode_refuses_veof_and_keeps_the_terminal_writable() {
        let _serial = run_serial();
        let transport = spawn(&spec("stty raw -echo; printf 'READY\\n'; sleep 30")).unwrap();
        read_until(&transport, b"READY", BUDGET);
        match transport.send_eof(Instant::now() + Duration::from_secs(5), None) {
            Err(EofError::NotCanonical) => {}
            other => panic!("raw mode must refuse VEOF, got {other:?}"),
        }
        write_all(&transport, b"x");
        transport.stop().unwrap();
    }

    #[test]
    fn cancellation_during_a_no_ready_read_poll_returns_promptly() {
        let _serial = run_serial();
        let transport = spawn(&spec("printf 'READY\\n'; sleep 30")).unwrap();
        read_until(&transport, b"READY", BUDGET);
        let cancelled = std::sync::Arc::new(AtomicBool::new(false));
        let probe = WaitProbe::install(cancelled.clone());
        let started = Instant::now();
        let outcome = transport
            .read_bytes(
                4096,
                Instant::now() + Duration::from_secs(30),
                Some(cancelled.as_ref()),
            )
            .unwrap();
        let elapsed = started.elapsed();
        assert!(outcome.cancelled, "{outcome:?}");
        assert!(
            !outcome.timed_out && outcome.bytes.is_empty(),
            "{outcome:?}"
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "cancellation was delayed inside the poll loop: {elapsed:?}"
        );
        drop(probe);
        transport.stop().unwrap();
    }

    #[test]
    fn a_blocked_write_stops_at_its_deadline_and_later_bytes_are_not_consumed() {
        let _serial = run_serial();
        let (_directory, gate, transport) = gated_reader();
        let data = vec![b'x'; MAX_INPUT_BYTES];
        let outcome = transport
            .write_bytes(WriteRequest {
                data: &data,
                deadline: Instant::now() + Duration::from_millis(300),
                cancelled: None,
                on_accepted: None,
            })
            .unwrap();
        assert!(outcome.timed_out, "{outcome:?}");
        assert!(
            outcome.accepted > 0,
            "the terminal accepted no bytes at all: {outcome:?}"
        );
        assert!(
            outcome.accepted < data.len(),
            "the terminal accepted the whole write: {outcome:?}"
        );
        assert_eq!(outcome.remaining, data.len() - outcome.accepted);
        // The child consumes exactly the accepted prefix; the sentinel proves
        // the queue was drained and that no writer continued afterwards.
        consume_and_collect(&transport, &gate, outcome.accepted, b'x');
        transport.stop().unwrap();
    }

    #[test]
    fn cancellation_during_a_no_ready_write_poll_stops_without_late_bytes() {
        let _serial = run_serial();
        let (_directory, gate, transport) = gated_reader();
        let data = vec![b'y'; MAX_INPUT_BYTES];
        let cancelled = std::sync::Arc::new(AtomicBool::new(false));
        let probe = WaitProbe::install(cancelled.clone());
        let started = Instant::now();
        let outcome = transport
            .write_bytes(WriteRequest {
                data: &data,
                deadline: Instant::now() + Duration::from_secs(30),
                cancelled: Some(cancelled.as_ref()),
                on_accepted: None,
            })
            .unwrap();
        let elapsed = started.elapsed();
        drop(probe);
        assert!(outcome.cancelled, "{outcome:?}");
        assert!(
            elapsed < Duration::from_secs(1),
            "cancellation was delayed inside the poll loop: {elapsed:?}"
        );
        assert!(
            outcome.accepted > 0 && outcome.accepted < data.len(),
            "{outcome:?}"
        );
        consume_and_collect(&transport, &gate, outcome.accepted, b'y');
        transport.stop().unwrap();
    }

    #[test]
    fn contending_writers_keep_their_own_deadline_and_cancellation() {
        let _serial = run_serial();
        let (_directory, gate, transport) = gated_reader();
        let data = vec![b'x'; MAX_INPUT_BYTES];
        let first_cancelled = std::sync::Arc::new(AtomicBool::new(false));
        let (accepted_tx, accepted_rx) = std::sync::mpsc::channel();
        let first = std::thread::scope(|scope| {
            let first_cancel = first_cancelled.clone();
            let transport_ref = &transport;
            let data_ref: &[u8] = &data;
            let handle = scope.spawn(move || {
                let mut notify = |_count: usize| {
                    let _ = accepted_tx.send(());
                };
                transport_ref
                    .write_bytes(WriteRequest {
                        data: data_ref,
                        deadline: Instant::now() + Duration::from_secs(10),
                        cancelled: Some(first_cancel.as_ref()),
                        on_accepted: Some(&mut notify as &mut dyn FnMut(usize)),
                    })
                    .unwrap()
            });
            // The first writer has accepted bytes, so it holds the gate.
            accepted_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("the first writer never accepted a chunk");
            let second_started = Instant::now();
            let second = transport
                .write_bytes(WriteRequest {
                    data: b"BBBBBBBBBBBBBBBB",
                    deadline: Instant::now() + Duration::from_millis(200),
                    cancelled: None,
                    on_accepted: None,
                })
                .unwrap();
            let second_elapsed = second_started.elapsed();
            assert!(second.timed_out, "{second:?}");
            assert_eq!(second.accepted, 0, "{second:?}");
            assert_eq!(second.remaining, 16);
            assert!(
                second_elapsed < Duration::from_secs(1),
                "the second writer waited behind the first: {second_elapsed:?}"
            );
            // A third caller is cancelled while it is actually waiting on the
            // gate: the probe fires only after a failed `try_lock`, so this is
            // a gate wait, not a poll wait.
            let gate_cancelled = std::sync::Arc::new(AtomicBool::new(false));
            let gate_probe = WaitProbe::install(gate_cancelled.clone());
            let gate_started = Instant::now();
            let third = transport
                .write_bytes(WriteRequest {
                    data: b"CCCC",
                    deadline: Instant::now() + Duration::from_secs(30),
                    cancelled: Some(gate_cancelled.as_ref()),
                    on_accepted: None,
                })
                .unwrap();
            let gate_elapsed = gate_started.elapsed();
            drop(gate_probe);
            assert!(third.cancelled, "{third:?}");
            assert_eq!(third.accepted, 0, "{third:?}");
            assert_eq!(third.remaining, 4);
            assert!(
                gate_elapsed < Duration::from_secs(1),
                "cancellation during the write-gate wait was delayed: {gate_elapsed:?}"
            );
            first_cancelled.store(true, Ordering::Release);
            handle.join().unwrap()
        });
        assert!(first.cancelled, "{first:?}");
        assert!(
            first.accepted > 0 && first.accepted < data.len(),
            "{first:?}"
        );
        let consumed = consume_and_collect(&transport, &gate, first.accepted, b'x');
        assert!(
            !consumed.contains(&b'B'),
            "the timed-out writer still reached the terminal"
        );
        assert!(
            !consumed.contains(&b'C'),
            "the gate-cancelled writer still reached the terminal"
        );
        transport.stop().unwrap();
    }

    #[test]
    fn explicit_stop_cleans_a_survivor_of_a_naturally_exited_leader() {
        let _serial = run_serial();
        let (_directory, transport, leader, survivor) = exited_leader_with_survivor();
        transport.stop().unwrap();
        wait_until_gone(survivor, Duration::from_secs(5));
        wait_until_gone(leader, Duration::from_secs(5));
    }

    #[test]
    fn dropping_the_transport_cleans_a_survivor_of_a_naturally_exited_leader() {
        let _serial = run_serial();
        let (_directory, transport, leader, survivor) = exited_leader_with_survivor();
        drop(transport);
        wait_until_gone(survivor, Duration::from_secs(5));
        wait_until_gone(leader, Duration::from_secs(5));
    }

    #[test]
    fn an_exited_child_reports_eof_without_blocking() {
        let _serial = run_serial();
        let transport = spawn(&spec("printf 'BYE\\n'; exit 3")).unwrap();
        let output = read_to_eof(&transport, BUDGET);
        assert!(String::from_utf8_lossy(&output).contains("BYE"));
        let again = transport
            .read_bytes(16, Instant::now() + Duration::from_millis(500), None)
            .unwrap();
        assert!(
            again.eof && again.bytes.is_empty() && !again.timed_out,
            "{again:?}"
        );
        let status = transport.stop().unwrap().expect("the child must be reaped");
        assert_eq!(status.exit_code(), 3);
        let repeat = transport
            .stop()
            .unwrap()
            .expect("repeated stop is idempotent");
        assert_eq!(repeat.exit_code(), 3);
    }

    #[test]
    fn read_requests_are_bounded() {
        let _serial = run_serial();
        let transport = spawn(&spec("printf 'READY\\n'; sleep 30")).unwrap();
        read_until(&transport, b"READY", BUDGET);
        let now = Instant::now();
        assert!(transport.read_bytes(0, now, None).is_err());
        assert!(transport.read_bytes(MAX_READ_BYTES + 1, now, None).is_err());
        transport.stop().unwrap();
    }

    #[test]
    fn explicit_stop_terminates_a_running_child_and_is_idempotent() {
        let _serial = run_serial();
        let transport = spawn(&spec("printf 'READY\\n'; sleep 30")).unwrap();
        read_until(&transport, b"READY", BUDGET);
        let pid = transport.process_id();
        assert!(process_alive(pid));
        assert!(transport.stop().unwrap().is_some());
        wait_until_gone(pid, Duration::from_secs(5));
        assert!(transport.stop().unwrap().is_some());
    }

    #[test]
    fn stop_signals_the_original_process_group() {
        let _serial = run_serial();
        let transport = spawn(&spec(
            "sleep 30 & printf 'GC=%s\\n' \"$!\"; printf 'READY\\n'; wait",
        ))
        .unwrap();
        let output = read_until(&transport, b"READY", BUDGET);
        let text = String::from_utf8_lossy(&output);
        let grandchild: u32 = text
            .split("GC=")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| panic!("no grandchild PID in {text:?}"));
        assert!(process_alive(grandchild), "the grandchild never started");
        let leader = transport.process_id();
        transport.stop().unwrap();
        wait_until_gone(grandchild, Duration::from_secs(5));
        wait_until_gone(leader, Duration::from_secs(5));
    }

    #[test]
    fn dropping_the_transport_stops_its_own_process_group() {
        let _serial = run_serial();
        let transport = spawn(&spec("printf 'READY\\n'; sleep 30")).unwrap();
        read_until(&transport, b"READY", BUDGET);
        let pid = transport.process_id();
        assert!(process_alive(pid));
        drop(transport);
        wait_until_gone(pid, Duration::from_secs(10));
    }
}
