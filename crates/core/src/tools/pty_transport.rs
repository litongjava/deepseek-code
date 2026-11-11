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
//! * an RAII guard that terminates the whole terminal session this transport
//!   started, then reaps its leader.
//!
//! Termination guarantee: portable-pty gives the child its own session with
//! `setsid()`, so the child is the session leader (`pid == pgid == sid`).
//! [`PtyTransport::stop`] and `Drop` terminate every live process whose
//! `/proc/<pid>/stat` session field equals that leader PID. That covers the
//! leader's own group, the additional process groups an interactive shell
//! creates for foreground and background jobs, and children forked while
//! cleanup is running.
//!
//! A session ID is only a usable identity while the leader is unreaped: the
//! leader's PID keeps the ID reserved, so no new session can appear under it.
//! The leader is therefore signalled like any other member but reaped only once
//! the session has no live members left.
//!
//! Boundary: a process that deliberately leaves the session (`setsid`, a
//! daemonising double fork, a service manager) is no longer in this session and
//! is not covered. Nothing here promises to find such a process.

use anyhow::{anyhow, bail, ensure, Context, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, ExitStatus, MasterPty, PtySize};
use rustix::fd::OwnedFd;
use std::{
    ffi::OsString,
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
    /// A stop was requested but cleanup is not confirmed yet, and this call
    /// found nothing queued. That is **not** end of output: a live child may
    /// still write, and a failed cleanup means the terminal may never close.
    /// Callers must not treat this as a final read.
    pub cleanup_pending: bool,
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

/// The direct PTY child, its session identity and the unreaped leader handle.
///
/// `session` is the child's own PID, because portable-pty calls `setsid()` in
/// the child. While the child is not reaped, that PID stays allocated and the
/// session ID stays reserved, so "session field equals `session`" is a precise
/// ownership test rather than a heuristic. Nothing is signalled by name or by
/// PID range. `Drop` cleans up only while [`Self::armed`], which a successful
/// [`Self::stop`] clears.
struct OwnedChild {
    child: Box<dyn Child + Send + Sync>,
    session: u32,
    finished: Option<ExitStatus>,
    armed: bool,
}

impl OwnedChild {
    fn new(child: Box<dyn Child + Send + Sync>, session: u32) -> Self {
        Self {
            child,
            session,
            finished: None,
            armed: true,
        }
    }

    fn process_id(&self) -> u32 {
        self.session
    }

    /// Terminate every live process in the owned terminal session, then reap the
    /// leader.
    ///
    /// Each pass signals the session's members through pidfds, re-inspects the
    /// session, and only reports success when no live member is left. That
    /// covers foreground and background job-control groups that are separate
    /// from the leader's group, and also a child forked while cleanup runs,
    /// because a forked child is found by the next pass.
    ///
    /// The leader is never reaped before the session is empty: reaping it would
    /// release the session ID and allow an unrelated new session to appear under
    /// the same ID while this cleanup still enumerates it. When the bound is
    /// exceeded, the error names the remaining PIDs and groups, and `armed`
    /// stays set so `Drop` or a later stop retries instead of reporting a clean
    /// shutdown.
    ///
    /// Calling it again returns the cached status, so repeat stops are equal.
    fn stop(&mut self) -> Result<Option<ExitStatus>> {
        self.stop_with(&ProcFs)
    }

    /// [`Self::stop`] against an injected view of the process table.
    ///
    /// The source parameter exists so tests can prove that an unreadable or
    /// unparsable record is reported as uncertainty instead of being skipped as
    /// a vanished process. Production always passes [`ProcFs`].
    fn stop_with(&mut self, source: &dyn ProcSource) -> Result<Option<ExitStatus>> {
        if let Some(status) = &self.finished {
            return Ok(Some(status.clone()));
        }
        let session = self.session;
        let deadline = Instant::now() + STOP_BOUND;
        loop {
            let signal_error = signal_session_with(source, session).err();
            // If this read fails, termination cannot be established. Do not
            // reap and do not clear `armed`.
            let members = session_members_with(source, session)
                .context("Reading the owned terminal session; cleanup cannot be verified")?;
            if members.is_empty() {
                return match self.reap(deadline) {
                    Ok(status) => Ok(status),
                    Err(error) => Err(match signal_error {
                        Some(signal_error) => error.context(format!("{signal_error:#}")),
                        None => error,
                    }),
                };
            }
            if Instant::now() >= deadline {
                let remaining: Vec<String> = members
                    .iter()
                    .map(|member| format!("PID {} in group {}", member.pid, member.group))
                    .collect();
                let mut error = anyhow!(
                    "The owned terminal session {session} still has {} live process(es) after {STOP_BOUND:?}: {}. The session was not terminated.",
                    members.len(),
                    remaining.join(", ")
                );
                if let Some(signal_error) = signal_error {
                    error = error.context(format!("{signal_error:#}"));
                }
                // `armed` stays true: Drop or a later stop must retry.
                return Err(error);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Reap the leader once the session has no live members.
    fn reap(&mut self, deadline: Instant) -> Result<Option<ExitStatus>> {
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
                    "The owned terminal session {} has no live processes, but its leader did not exit within {STOP_BOUND:?}",
                    self.session
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
    /// Set when [`PtyTransport::stop`] begins. Blocked reads and writes observe
    /// it within one poll slice, so writes end deterministically and reads stop
    /// waiting for new output.
    stop_requested: AtomicBool,
    /// Set only after the owned session was verified empty and the leader was
    /// reaped. Until then an empty read means "nothing queued yet", not "output
    /// is final": a child can still be alive and write while cleanup runs.
    cleanup_done: AtomicBool,
    /// Test-only pause between the stop request and signalling, so a test can
    /// inspect the pending window deterministically. Never present in a release
    /// build.
    #[cfg(test)]
    stop_hook: Mutex<Option<Box<dyn FnMut() + Send>>>,
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
                    cleanup_pending: false,
                });
            }
            if self.stop_requested.load(Ordering::Acquire) {
                return self.read_after_stop(max);
            }
            match wait_for(
                &self.read_fd,
                rustix::event::PollFlags::IN,
                deadline,
                cancelled,
                &self.stop_requested,
            )? {
                Waited::Cancelled => {
                    return Ok(ReadOutcome {
                        bytes: Vec::new(),
                        eof: false,
                        timed_out: false,
                        cancelled: true,
                        cleanup_pending: false,
                    })
                }
                Waited::TimedOut => {
                    return Ok(ReadOutcome {
                        bytes: Vec::new(),
                        eof: false,
                        timed_out: true,
                        cancelled: false,
                        cleanup_pending: false,
                    })
                }
                // A stopped transport stops waiting for new output, but it does
                // not throw away what the terminal already queued: one
                // nonblocking read returns that tail so a caller can still see
                // final diagnostics before EOF.
                Waited::Stopped => return self.read_after_stop(max),
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
                        cleanup_pending: false,
                    })
                }
                Ok(count) => {
                    buffer.truncate(count);
                    return Ok(ReadOutcome {
                        bytes: buffer,
                        eof: false,
                        timed_out: false,
                        cancelled: false,
                        cleanup_pending: false,
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
                        cleanup_pending: false,
                    })
                }
                Err(error) => return Err(error).context("Reading from the PTY master"),
            }
        }
    }

    /// Read what the terminal had already queued, without waiting.
    ///
    /// This is the post-stop read path. It performs one nonblocking `read(2)`
    /// and returns at most `max` bytes, so a caller loops until a final outcome
    /// to collect the whole tail; `max` still bounds one call, so draining is
    /// page-capped.
    ///
    /// Finality is deliberately conservative:
    ///
    /// * a zero-byte read or `EIO` is real terminal closure, so `eof` is true;
    /// * `EAGAIN` while cleanup is confirmed is also final, because the session
    ///   was verified empty and the leader reaped;
    /// * `EAGAIN` while cleanup is still pending (or failed) is **not** final:
    ///   `cleanup_pending` is set and `eof` stays false, because a live child may
    ///   still write and a failed cleanup may never close the terminal.
    fn read_after_stop(&self, max: usize) -> Result<ReadOutcome> {
        let mut buffer = vec![0u8; max];
        loop {
            match rustix::io::read(&self.read_fd, &mut buffer[..]) {
                Ok(0) => {
                    return Ok(ReadOutcome {
                        bytes: Vec::new(),
                        eof: true,
                        timed_out: false,
                        cancelled: false,
                        cleanup_pending: false,
                    })
                }
                Ok(count) => {
                    buffer.truncate(count);
                    return Ok(ReadOutcome {
                        bytes: buffer,
                        eof: false,
                        timed_out: false,
                        cancelled: false,
                        cleanup_pending: false,
                    });
                }
                Err(rustix::io::Errno::AGAIN) => {
                    let confirmed = self.cleanup_done.load(Ordering::Acquire);
                    return Ok(ReadOutcome {
                        bytes: Vec::new(),
                        eof: confirmed,
                        timed_out: false,
                        cancelled: false,
                        cleanup_pending: !confirmed,
                    });
                }
                Err(rustix::io::Errno::INTR) => continue,
                // The slave side is gone, which is EOF once the queue is empty.
                Err(rustix::io::Errno::IO) => {
                    return Ok(ReadOutcome {
                        bytes: Vec::new(),
                        eof: true,
                        timed_out: false,
                        cancelled: false,
                        cleanup_pending: false,
                    })
                }
                Err(error) => return Err(error).context("Reading buffered PTY output after stop"),
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
            if self.stop_requested.load(Ordering::Acquire) {
                return Gate::Stopped;
            }
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
            Gate::Stopped => {
                return Ok(WriteOutcome {
                    accepted: 0,
                    remaining: total,
                    timed_out: false,
                    cancelled: false,
                    closed: true,
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
                &self.stop_requested,
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
                // Stopped: the session is being torn down. Report exactly the
                // bytes the terminal took and never write anything else.
                Waited::Stopped => {
                    return Ok(WriteOutcome {
                        accepted,
                        remaining: total - accepted,
                        timed_out: false,
                        cancelled: false,
                        closed: true,
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

    /// Terminate the owned terminal session and return the leader's exit status.
    ///
    /// `stop_requested` is published before cleanup begins: concurrent writes
    /// end as `closed` with the bytes they already handed to the terminal, and
    /// concurrent reads stop waiting for new output but still return whatever
    /// the terminal queued. `cleanup_done` is published only after the session
    /// was verified empty and the leader reaped, which is what makes an empty
    /// read final. Calling `stop` again returns the cached status; a failed
    /// cleanup is retried, not reported as success.
    pub(crate) fn stop(&self) -> Result<Option<ExitStatus>> {
        self.stop_requested.store(true, Ordering::Release);
        #[cfg(test)]
        self.run_stop_hook();
        let result = lock(&self.child).stop();
        if result.is_ok() {
            self.cleanup_done.store(true, Ordering::Release);
        }
        result
    }

    /// Run the test-only pause installed by [`Self::install_stop_hook`].
    #[cfg(test)]
    fn run_stop_hook(&self) {
        let hook = lock(&self.stop_hook).take();
        if let Some(mut hook) = hook {
            hook();
        }
    }

    /// Install a one-shot callback between "stop requested" and "cleanup
    /// starts". Test-only: it lets a test inspect the pending window
    /// deterministically instead of racing a millisecond.
    #[cfg(test)]
    fn install_stop_hook(&self, hook: Box<dyn FnMut() + Send>) {
        *lock(&self.stop_hook) = Some(hook);
    }

    /// The terminal's current foreground process group (`tcgetpgrp`).
    ///
    /// This is diagnostic only. Cleanup never uses it as a containment proof:
    /// an interactive shell keeps only one group in the foreground while other
    /// job groups are alive in the background.
    #[cfg(test)]
    fn foreground_group(&self) -> Option<u32> {
        lock(&self.master)
            .process_group_leader()
            .map(|pid| pid as u32)
    }

    /// The slave device path, used by tests to prove the descriptor was closed.
    #[cfg(test)]
    fn tty_path(&self) -> Option<PathBuf> {
        lock(&self.master).tty_name()
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
        stop_requested: AtomicBool::new(false),
        cleanup_done: AtomicBool::new(false),
        #[cfg(test)]
        stop_hook: Mutex::new(None),
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

/// One live process observed inside the owned terminal session.
#[derive(Debug, Clone, PartialEq, Eq)]
struct SessionMember {
    pid: u32,
    group: u32,
    start: u64,
}

/// The `/proc/<pid>/stat` fields this module needs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcStat {
    state: char,
    group: u32,
    session: u32,
    start: u64,
}

/// Parse the fields of `/proc/<pid>/stat` that follow the command name.
///
/// The command name is parenthesised and may contain spaces, `)` or `(`, so
/// everything is taken after the last `)`. Field 22 is the creation time in
/// clock ticks; it is the only per-process value that distinguishes two
/// processes that held the same PID. `None` means the record is not parsable,
/// which callers treat as "unknown", never as "gone".
fn parse_proc_stat(text: &str) -> Option<ProcStat> {
    let (_, rest) = text.rsplit_once(')')?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    if fields.len() < 20 {
        return None;
    }
    Some(ProcStat {
        state: fields[0].chars().next()?,
        group: fields[2].parse().ok()?,
        session: fields[3].parse().ok()?,
        start: fields[19].parse().ok()?,
    })
}

/// Where the facts used for session ownership come from.
///
/// The production implementation reads `/proc`. Tests inject a source that
/// fails in specific ways, so "cannot read" is proven to be distinct from "the
/// process is gone" without weakening the real reader or falling back to
/// process names.
trait ProcSource {
    /// PID entries currently listed in `/proc`.
    fn list_pids(&self) -> Result<Vec<u32>>;
    /// The stat fields of one process.
    ///
    /// `Ok(None)` means the process no longer exists (`ENOENT`), which is the
    /// normal race against a member exiting. `Err` means its state could not be
    /// established (permission, I/O, unparsable record); cleanup must treat
    /// that as unknown rather than as disappearance.
    fn stat(&self, pid: u32) -> Result<Option<ProcStat>>;
}

struct ProcFs;

impl ProcSource for ProcFs {
    fn list_pids(&self) -> Result<Vec<u32>> {
        let entries = std::fs::read_dir("/proc")
            .context("Listing /proc to find session members; ownership cannot be verified")?;
        let mut pids = Vec::new();
        for entry in entries {
            // An entry that cannot be read is an unknown process, not an
            // absent one: an empty-looking session could hide a live member.
            let entry =
                entry.context("Reading one /proc entry; session membership cannot be verified")?;
            if let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<u32>().ok())
            {
                pids.push(pid);
            }
        }
        Ok(pids)
    }

    fn stat(&self, pid: u32) -> Result<Option<ProcStat>> {
        let text = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(text) => text,
            // The only expected failure is that the process exited between the
            // listing and this read.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("Reading /proc/{pid}/stat; its session cannot be verified")
                })
            }
        };
        let stat = parse_proc_stat(&text).ok_or_else(|| {
            anyhow!("The /proc/{pid}/stat record is not readable as a process stat line")
        })?;
        Ok(Some(stat))
    }
}

/// Every live process whose recorded session is exactly `session`, excluding
/// zombies.
///
/// The session ID is the unreaped leader's PID, so it names no other session.
/// Zombies are excluded because they cannot run, fork or use the terminal; the
/// leader's own zombie is reaped by [`OwnedChild::reap`]. A process that exited
/// between listing and reading is skipped; any other read failure is returned,
/// because it leaves ownership unverified.
fn session_members_with(source: &dyn ProcSource, session: u32) -> Result<Vec<SessionMember>> {
    let mut members = Vec::new();
    for pid in source.list_pids()? {
        let Some(stat) = source.stat(pid)? else {
            continue;
        };
        if stat.session != session {
            continue;
        }
        if matches!(stat.state, 'Z' | 'X' | 'x') {
            continue;
        }
        members.push(SessionMember {
            pid,
            group: stat.group,
            start: stat.start,
        });
    }
    members.sort_by_key(|member| member.pid);
    Ok(members)
}

fn session_members(session: u32) -> Result<Vec<SessionMember>> {
    session_members_with(&ProcFs, session)
}

/// Signal one observed session member through a pidfd.
///
/// The pidfd pins the process identity, and the identity (creation tick and
/// session) is re-read after the open, so a PID that was recycled into another
/// session between observation and signalling is skipped rather than killed.
/// `Ok(false)` means the process is gone or is no longer the one observed; an
/// identity that cannot be re-read is an error, because signalling would be
/// based on an unverified identity.
fn signal_member_with(
    source: &dyn ProcSource,
    session: u32,
    member: &SessionMember,
) -> Result<bool> {
    let Some(pid) = rustix::process::Pid::from_raw(member.pid as i32) else {
        bail!("PID {} is not a valid process ID", member.pid);
    };
    let pidfd = match rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()) {
        Ok(pidfd) => pidfd,
        Err(rustix::io::Errno::SRCH) => return Ok(false),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Opening a descriptor for PID {}", member.pid))
        }
    };
    let stat = match source.stat(member.pid) {
        Ok(Some(stat)) => stat,
        // Gone between the listing and the open: nothing to signal.
        Ok(None) => return Ok(false),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "Verifying PID {} before signalling; its identity is unknown",
                    member.pid
                )
            })
        }
    };
    if stat.start != member.start || stat.session != session {
        return Ok(false);
    }
    match rustix::process::pidfd_send_signal(&pidfd, rustix::process::Signal::KILL) {
        Ok(()) => Ok(true),
        Err(rustix::io::Errno::SRCH) => Ok(false),
        Err(error) => Err(error).with_context(|| format!("Signalling PID {}", member.pid)),
    }
}

/// Signal every live process in the owned session and report how many accepted
/// a signal. A failure names the members that could not be verified or
/// signalled; the caller must then treat the session as unverified.
fn signal_session_with(source: &dyn ProcSource, session: u32) -> Result<usize> {
    let mut signalled = 0usize;
    let mut failures = Vec::new();
    for member in session_members_with(source, session)? {
        match signal_member_with(source, session, &member) {
            Ok(true) => signalled += 1,
            Ok(false) => {}
            Err(error) => failures.push(format!("#{error}")),
        }
    }
    if !failures.is_empty() {
        bail!(
            "Could not signal every process in the owned terminal session {session}: {}",
            failures.join("; ")
        );
    }
    Ok(signalled)
}

/// Why a wait ended.
enum Waited {
    Ready,
    TimedOut,
    Cancelled,
    Stopped,
}

/// Result of trying to take the write gate within one call's own budget.
enum Gate<'a> {
    Acquired(std::sync::MutexGuard<'a, ()>),
    TimedOut,
    Cancelled,
    Stopped,
}

/// Wait until `fd` is ready, the deadline passes, cancellation is observed, or
/// the transport is stopped.
///
/// The loop is sliced so every condition is noticed at most one `POLL_SLICE`
/// after it becomes true, even while the terminal never becomes ready. A ready
/// descriptor may still report HUP/ERR; the caller's read or write turns that
/// into EOF or EIO.
fn wait_for(
    fd: &OwnedFd,
    events: rustix::event::PollFlags,
    deadline: Instant,
    cancelled: Option<&AtomicBool>,
    stop_requested: &AtomicBool,
) -> Result<Waited> {
    loop {
        if stop_requested.load(Ordering::Acquire) {
            return Ok(Waited::Stopped);
        }
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
    fn gated_reader() -> (SessionFixture, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let gate = directory.path().join("gate");
        let transport = spawn(&spec(&format!(
            "stty raw -echo; printf 'READY\\n'; while [ ! -e '{}' ]; do sleep 0.01; done; cat",
            gate.display()
        )))
        .unwrap();
        read_until(&transport, b"READY", BUDGET);
        (SessionFixture::new(transport, Some(directory)), gate)
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
        group_of_pid(pid)
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

    /// Test-owned cleanup for one fixture session.
    ///
    /// Deliberately independent of [`signal_session`]: it still works when a
    /// negative control temporarily breaks production cleanup. It signals only
    /// processes whose `/proc` session field equals this fixture's own session,
    /// each through its own pidfd, and it is disarmed as soon as a production
    /// stop has succeeded. After that stop the leader is reaped and its session
    /// ID could be reused, and this guard must never touch another session.
    struct SessionGuard {
        session: u32,
        armed: bool,
    }

    impl SessionGuard {
        fn new(session: u32) -> Self {
            Self {
                session,
                armed: true,
            }
        }

        fn disarm(&mut self) {
            self.armed = false;
        }
    }

    impl Drop for SessionGuard {
        fn drop(&mut self) {
            if !self.armed {
                return;
            }
            for member in session_members(self.session).unwrap_or_default() {
                let Some(pid) = rustix::process::Pid::from_raw(member.pid as i32) else {
                    continue;
                };
                let Ok(pidfd) =
                    rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty())
                else {
                    continue;
                };
                let _ = rustix::process::pidfd_send_signal(&pidfd, rustix::process::Signal::KILL);
            }
        }
    }

    /// A spawned transport plus the safety net for its session.
    ///
    /// Fields drop in declaration order, so the transport's own cleanup runs
    /// before the guard sweeps whatever a failed assertion left behind.
    struct SessionFixture {
        transport: PtyTransport,
        guard: SessionGuard,
        _directory: Option<tempfile::TempDir>,
    }

    impl SessionFixture {
        fn new(transport: PtyTransport, directory: Option<tempfile::TempDir>) -> Self {
            let guard = SessionGuard::new(transport.process_id());
            Self {
                transport,
                guard,
                _directory: directory,
            }
        }

        /// Stop through the production path. The safety net stays armed unless
        /// the session is verifiably empty afterwards, so a stop that reports
        /// success while job groups survive still gets cleaned on unwind.
        fn stop(&mut self) -> Result<Option<ExitStatus>> {
            let status = self.transport.stop()?;
            let verified_empty = session_members(self.transport.process_id())
                .map(|members| members.is_empty())
                .unwrap_or(false);
            if verified_empty {
                self.guard.disarm();
            }
            Ok(status)
        }
    }

    /// A process table for tests, with explicitly injected failures.
    struct FakeProc {
        pids: Vec<u32>,
        stats: std::collections::HashMap<u32, ProcStat>,
        unreadable: std::collections::HashSet<u32>,
        listing_fails: bool,
    }

    impl FakeProc {
        fn new() -> Self {
            Self {
                pids: Vec::new(),
                stats: std::collections::HashMap::new(),
                unreadable: std::collections::HashSet::new(),
                listing_fails: false,
            }
        }

        fn member(mut self, pid: u32, group: u32, session: u32) -> Self {
            self.pids.push(pid);
            self.stats.insert(
                pid,
                ProcStat {
                    state: 'S',
                    group,
                    session,
                    start: 1000 + pid as u64,
                },
            );
            self
        }

        fn unreadable(mut self, pid: u32) -> Self {
            self.unreadable.insert(pid);
            self
        }
    }

    impl ProcSource for FakeProc {
        fn list_pids(&self) -> Result<Vec<u32>> {
            if self.listing_fails {
                bail!("injected /proc listing failure");
            }
            Ok(self.pids.clone())
        }

        fn stat(&self, pid: u32) -> Result<Option<ProcStat>> {
            if self.unreadable.contains(&pid) {
                bail!("injected permission failure for PID {pid}");
            }
            Ok(self.stats.get(&pid).cloned())
        }
    }

    fn session_of(pid: u32) -> Option<u32> {
        ProcFs.stat(pid).ok().flatten().map(|stat| stat.session)
    }

    fn group_of_pid(pid: u32) -> Option<u32> {
        ProcFs.stat(pid).ok().flatten().map(|stat| stat.group)
    }

    /// The number printed by `<marker><digits>` once the whole number has
    /// arrived. A partial number is not a value yet.
    fn marker_value(text: &str, marker: &str) -> Option<u32> {
        let start = text.find(marker)? + marker.len();
        let digits: String = text[start..]
            .chars()
            .take_while(|character| character.is_ascii_digit())
            .collect();
        digits.parse().ok()
    }

    /// Read until every marker has a complete number, or fail with what arrived.
    fn read_until_markers(transport: &PtyTransport, markers: &[&str], budget: Duration) -> String {
        let deadline = Instant::now() + budget;
        let mut collected = Vec::new();
        loop {
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {markers:?}; collected {:?}",
                    String::from_utf8_lossy(&collected)
                );
            }
            let outcome = transport.read_bytes(4096, deadline, None).unwrap();
            collected.extend_from_slice(&outcome.bytes);
            let text = String::from_utf8_lossy(&collected).into_owned();
            if markers
                .iter()
                .all(|marker| marker_value(&text, marker).is_some())
            {
                return text;
            }
            if outcome.eof {
                panic!(
                    "EOF before {markers:?}; collected {:?}",
                    String::from_utf8_lossy(&collected)
                );
            }
        }
    }

    /// A bash with job control enabled inside the PTY.
    ///
    /// The script reports the PID of a background command and of a background
    /// pipeline, then blocks in a foreground command. Bash's `set -m` puts each
    /// job in its own process group while they all stay in the session created
    /// by portable-pty, which is exactly the layout that a leader-PG-only
    /// cleanup misses.
    fn job_control_fixture() -> (SessionFixture, u32, u32, u32) {
        let directory = tempfile::tempdir().unwrap();
        let script = "set -m\n\
                      printf 'READY\\n'\n\
                      sleep 30 & printf 'BG=%s\\n' \"$!\"\n\
                      sleep 30 | cat & printf 'PIPE=%s\\n' \"$!\"\n\
                      sleep 30\n\
                      printf 'FG-DONE\\n'\n";
        let transport = spawn(&SpawnSpec {
            executable: OsString::from("/bin/bash"),
            // Interactive job control is what gives the foreground command its
            // own process group and the terminal (`tcsetpgrp`); a plain `-c`
            // shell may simply exec its last command in the leader's group.
            arguments: vec![
                OsString::from("--noprofile"),
                OsString::from("--norc"),
                OsString::from("-i"),
                OsString::from("-c"),
                OsString::from(script),
            ],
            cwd: directory.path().to_path_buf(),
            environment: vec![(OsString::from("PATH"), OsString::from("/usr/bin:/bin"))],
            rows: 24,
            cols: 80,
        })
        .unwrap();
        let output = read_until_markers(&transport, &["BG=", "PIPE="], BUDGET);
        let leader = transport.process_id();
        let background = marker_value(&output, "BG=").unwrap();
        let pipeline = marker_value(&output, "PIPE=").unwrap();
        println!(
            "job-control fixture: leader={leader} background={background} pipeline={pipeline}"
        );
        (
            SessionFixture::new(transport, Some(directory)),
            leader,
            background,
            pipeline,
        )
    }

    /// The terminal's foreground process group once the foreground job has taken
    /// it, bounded rather than guessed.
    fn wait_for_foreground_group(transport: &PtyTransport, leader: u32, budget: Duration) -> u32 {
        let deadline = Instant::now() + budget;
        loop {
            match transport.foreground_group() {
                Some(group) if group != leader => return group,
                _ if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                other => panic!("the foreground job never took the terminal: {other:?}"),
            }
        }
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
        let (mut fixture, gate) = gated_reader();
        let transport = &fixture.transport;
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
        consume_and_collect(transport, &gate, outcome.accepted, b'x');
        fixture.stop().unwrap();
    }

    #[test]
    fn cancellation_during_a_no_ready_write_poll_stops_without_late_bytes() {
        let _serial = run_serial();
        let (mut fixture, gate) = gated_reader();
        let transport = &fixture.transport;
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
        consume_and_collect(transport, &gate, outcome.accepted, b'y');
        fixture.stop().unwrap();
    }

    #[test]
    fn contending_writers_keep_their_own_deadline_and_cancellation() {
        let _serial = run_serial();
        let (mut fixture, gate) = gated_reader();
        let transport = &fixture.transport;
        let data = vec![b'x'; MAX_INPUT_BYTES];
        let first_cancelled = std::sync::Arc::new(AtomicBool::new(false));
        let (accepted_tx, accepted_rx) = std::sync::mpsc::channel();
        let first = std::thread::scope(|scope| {
            let first_cancel = first_cancelled.clone();
            let transport_ref = transport;
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
        let consumed = consume_and_collect(transport, &gate, first.accepted, b'x');
        assert!(
            !consumed.contains(&b'B'),
            "the timed-out writer still reached the terminal"
        );
        assert!(
            !consumed.contains(&b'C'),
            "the gate-cancelled writer still reached the terminal"
        );
        fixture.stop().unwrap();
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

    #[test]
    fn interactive_job_control_creates_distinct_groups_in_one_session() {
        let _serial = run_serial();
        let (mut fixture, leader, background, pipeline) = job_control_fixture();
        let transport = &fixture.transport;

        assert_eq!(session_of(leader), Some(leader));
        assert_eq!(
            session_of(background),
            Some(leader),
            "the background job is not in the owned session"
        );
        assert_eq!(
            session_of(pipeline),
            Some(leader),
            "the pipeline is not in the owned session"
        );

        let background_group = group_of_pid(background).expect("a background group");
        let pipeline_group = group_of_pid(pipeline).expect("a pipeline group");
        assert_ne!(
            background_group, leader,
            "the background job stayed in the leader's group"
        );
        assert_ne!(
            pipeline_group, leader,
            "the pipeline stayed in the leader's group"
        );
        assert_ne!(background_group, pipeline_group);

        // The foreground command is in a third group and owns the terminal.
        let foreground = wait_for_foreground_group(transport, leader, BUDGET);
        assert_ne!(foreground, background_group);
        assert_ne!(foreground, pipeline_group);
        let members = session_members(leader).unwrap();
        assert!(
            members.iter().any(|member| member.group == foreground),
            "no live process backs the foreground group {foreground}: {members:?}"
        );
        assert!(
            members
                .iter()
                .any(|member| member.group == background_group),
            "the background group is missing from the session: {members:?}"
        );

        fixture.stop().unwrap();
    }

    #[test]
    fn stop_cleans_foreground_and_background_job_groups() {
        let _serial = run_serial();
        let (mut fixture, leader, background, pipeline) = job_control_fixture();
        let foreground = wait_for_foreground_group(&fixture.transport, leader, BUDGET);
        assert!(
            !session_members(leader).unwrap().is_empty(),
            "the fixture has no session members to clean"
        );

        let first = fixture.stop().unwrap().expect("the leader was reaped");
        wait_until_gone(leader, Duration::from_secs(5));
        wait_until_gone(background, Duration::from_secs(5));
        wait_until_gone(pipeline, Duration::from_secs(5));
        let members = session_members(leader).unwrap();
        assert!(
            members.is_empty(),
            "the session still has live members after stop: {members:?}"
        );
        assert!(
            !members.iter().any(|member| member.group == foreground),
            "the foreground group {foreground} survived"
        );

        let second = fixture.stop().unwrap().expect("cached status");
        assert_eq!(first.exit_code(), second.exit_code());
    }

    #[test]
    fn a_child_forked_during_stop_is_also_cleaned() {
        let _serial = run_serial();
        let transport = spawn(&SpawnSpec {
            executable: OsString::from("/bin/bash"),
            arguments: vec![
                OsString::from("-c"),
                OsString::from(
                    "set -m\n\
                     printf 'READY\\n'\n\
                     while :; do sleep 30 & printf 'FORK=%s\\n' \"$!\"; wait $!; done\n",
                ),
            ],
            cwd: std::env::temp_dir(),
            environment: vec![(OsString::from("PATH"), OsString::from("/usr/bin:/bin"))],
            rows: 24,
            cols: 80,
        })
        .unwrap();
        let mut fixture = SessionFixture::new(transport, None);
        let transport = &fixture.transport;
        let output = read_until_markers(transport, &["FORK="], BUDGET);
        let leader = transport.process_id();
        let forked = marker_value(&output, "FORK=").unwrap();
        println!("fork fixture: leader={leader} forked={forked}");
        assert_eq!(session_of(forked), Some(leader));
        assert_ne!(
            group_of_pid(forked),
            Some(leader),
            "the forked job is not in its own process group"
        );

        // The loop keeps starting jobs, so cleanup has to find processes that
        // appear after its first pass.
        fixture.stop().unwrap();
        wait_until_gone(forked, Duration::from_secs(5));
        wait_until_gone(leader, Duration::from_secs(5));
        let members = session_members(leader).unwrap();
        assert!(
            members.is_empty(),
            "a forked job survived stop: {members:?}"
        );
    }

    #[test]
    fn stopping_one_session_leaves_other_sessions_and_processes_alive() {
        let _serial = run_serial();
        let (mut fixture, leader, _background, _pipeline) = job_control_fixture();
        let mut other_fixture =
            SessionFixture::new(spawn(&spec("printf 'READY\\n'; sleep 30")).unwrap(), None);
        let other = &other_fixture.transport;
        read_until(other, b"READY", BUDGET);
        let other_pid = other.process_id();
        assert_eq!(
            session_of(other_pid),
            Some(other_pid),
            "the control transport did not get its own session"
        );
        let mut plain = std::process::Command::new("sleep")
            .arg("30")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let plain_pid = plain.id();

        fixture.stop().unwrap();
        wait_until_gone(leader, Duration::from_secs(5));
        assert!(session_members(leader).unwrap().is_empty());
        assert!(
            process_alive(other_pid),
            "cleanup touched the other terminal session"
        );
        assert!(
            process_alive(plain_pid),
            "cleanup touched a process outside the owned session"
        );
        assert_ne!(session_of(plain_pid), Some(leader));

        other_fixture.stop().unwrap();
        wait_until_gone(other_pid, Duration::from_secs(5));
        let _ = plain.kill();
        let _ = plain.wait();
        wait_until_gone(plain_pid, Duration::from_secs(5));
    }

    #[test]
    fn stop_closes_a_blocked_write_without_late_bytes() {
        let _serial = run_serial();
        let (fixture, _gate) = gated_reader();
        // The guard and the transport are taken apart so that the nested scope
        // can stop the transport while the guard stays armed for an unwinding
        // assertion, then disarms it only after `stop` succeeded.
        let SessionFixture {
            transport,
            mut guard,
            ..
        } = fixture;
        let data = vec![b'z'; MAX_INPUT_BYTES];
        let (accepted_tx, accepted_rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let transport_ref = &transport;
            let data_ref: &[u8] = &data;
            let handle = scope.spawn(move || {
                let mut notify = |_count: usize| {
                    let _ = accepted_tx.send(());
                };
                transport_ref
                    .write_bytes(WriteRequest {
                        data: data_ref,
                        deadline: Instant::now() + Duration::from_secs(30),
                        cancelled: None,
                        on_accepted: Some(&mut notify as &mut dyn FnMut(usize)),
                    })
                    .unwrap()
            });
            accepted_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("the write never accepted a chunk while the terminal was full");
            let started = Instant::now();
            transport.stop().unwrap();
            guard.disarm();
            let write = handle.join().unwrap();
            let elapsed = started.elapsed();
            assert!(write.closed, "{write:?}");
            assert!(write.accepted > 0, "{write:?}");
            assert_eq!(write.remaining, data.len() - write.accepted);
            assert!(
                elapsed < Duration::from_secs(5),
                "stop did not close the blocked write: {elapsed:?}"
            );
        });

        // A stopped transport still drains what the terminal queued, then
        // reports EOF. This fixture wrote nothing after READY, so the tail is
        // empty. A later write is refused with zero accepted bytes.
        let mut drained = Vec::new();
        loop {
            let read = transport
                .read_bytes(4096, Instant::now() + Duration::from_secs(5), None)
                .unwrap();
            drained.extend_from_slice(&read.bytes);
            if read.eof {
                break;
            }
        }
        assert!(
            drained.is_empty(),
            "the fixture wrote no tail, but the drain returned {drained:?}"
        );
        let late = transport
            .write_bytes(WriteRequest {
                data: b"late bytes",
                deadline: Instant::now() + Duration::from_secs(5),
                cancelled: None,
                on_accepted: None,
            })
            .unwrap();
        assert!(late.closed, "{late:?}");
        assert_eq!(late.accepted, 0, "{late:?}");
        assert_eq!(late.remaining, "late bytes".len());
    }

    /// Output the child queued before it was stopped must still be readable
    /// after stop, exactly once, and the drain must stay page-capped.
    #[test]
    fn buffered_output_survives_stop_and_drains_exactly_once() {
        let _serial = run_serial();
        let directory = tempfile::tempdir().unwrap();
        let ready = directory.path().join("ready");
        let script = format!(
            "stty raw -echo\n\
             printf 'BEGIN-'\n\
             i=0\n\
             while [ \"$i\" -lt 200 ]; do printf '0123456789abcdef'; i=$((i+1)); done\n\
             printf '%s\\n' '-END'\n\
             : > '{}'\n\
             sleep 30\n",
            ready.display()
        );
        let mut fixture = SessionFixture::new(
            spawn(&SpawnSpec {
                executable: OsString::from("/bin/sh"),
                arguments: vec![OsString::from("-c"), OsString::from(script)],
                cwd: directory.path().to_path_buf(),
                environment: vec![(OsString::from("PATH"), OsString::from("/usr/bin:/bin"))],
                rows: 24,
                cols: 80,
            })
            .unwrap(),
            Some(directory),
        );

        // The ready marker is a side channel, so the tail stays unread in the
        // terminal queue when stop begins.
        let deadline = Instant::now() + BUDGET;
        while !ready.exists() {
            assert!(Instant::now() < deadline, "the child never queued its tail");
            std::thread::sleep(Duration::from_millis(10));
        }
        fixture.stop().unwrap();

        let mut expected = b"BEGIN-".to_vec();
        for _ in 0..200 {
            expected.extend_from_slice(b"0123456789abcdef");
        }
        expected.extend_from_slice(b"-END\n");

        // Page cap: no single call returns more than it was asked for.
        let first = fixture
            .transport
            .read_bytes(1024, Instant::now() + Duration::from_secs(5), None)
            .unwrap();
        assert_eq!(first.bytes.len(), 1024, "the queue was not page-capped");
        assert!(
            !first.eof,
            "a partially drained queue must not report EOF yet"
        );
        let mut collected = first.bytes.clone();
        // Drain the rest with the documented page size; the drain reports EOF
        // only in the call that finds the queue empty.
        loop {
            let read = fixture
                .transport
                .read_bytes(4096, Instant::now() + Duration::from_secs(5), None)
                .unwrap();
            collected.extend_from_slice(&read.bytes);
            if read.eof {
                break;
            }
        }
        assert_eq!(collected, expected, "the queued tail changed or repeated");
        // Once drained, the queue stays empty: the bytes are not replayed.
        let again = fixture
            .transport
            .read_bytes(4096, Instant::now() + Duration::from_secs(1), None)
            .unwrap();
        assert!(again.eof && again.bytes.is_empty(), "{again:?}");
    }

    #[test]
    fn repeated_stop_is_idempotent_and_releases_the_terminal() {
        let _serial = run_serial();
        let mut fixture =
            SessionFixture::new(spawn(&spec("printf 'READY\\n'; sleep 30")).unwrap(), None);
        read_until(&fixture.transport, b"READY", BUDGET);
        let tty = fixture
            .transport
            .tty_path()
            .expect("the transport has a tty path");
        let leader = fixture.transport.process_id();

        let first = fixture.stop().unwrap().expect("the leader was reaped");
        let second = fixture.stop().unwrap().expect("cached status");
        assert_eq!(first.exit_code(), second.exit_code());
        wait_until_gone(leader, Duration::from_secs(5));
        assert!(session_members(leader).unwrap().is_empty());

        // Every descriptor of this process that referred to the slave is closed
        // by the time the transport is dropped; no pty fd outlives it.
        drop(fixture);
        let leaked: Vec<PathBuf> = std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .flatten()
            .filter_map(|entry| std::fs::read_link(entry.path()).ok())
            .filter(|target| target == &tty)
            .collect();
        assert!(
            leaked.is_empty(),
            "the terminal {} is still open after drop",
            tty.display()
        );
    }

    #[test]
    fn a_malformed_stat_record_is_unknown_not_gone() {
        assert!(parse_proc_stat("").is_none());
        assert!(parse_proc_stat("1234 (bash) R").is_none());
        assert!(parse_proc_stat("1234 (bash) R 1 notanumber").is_none());
        let stat =
            parse_proc_stat("1234 (a b) S 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 999 20")
                .expect("a complete record parses");
        assert_eq!(stat.state, 'S');
        assert_eq!(stat.group, 2);
        assert_eq!(stat.session, 3);
        assert_eq!(stat.start, 999);
    }

    #[test]
    fn an_unreadable_member_is_not_treated_as_vanished() {
        let source = FakeProc::new().member(41, 41, 41).unreadable(41);
        let error = session_members_with(&source, 41).unwrap_err();
        assert!(
            format!("{error:#}").contains("injected permission failure"),
            "{error:#}"
        );
    }

    #[test]
    fn a_listing_failure_is_an_error() {
        let mut source = FakeProc::new();
        source.listing_fails = true;
        assert!(session_members_with(&source, 41).is_err());
        assert!(signal_session_with(&source, 41).is_err());
    }

    #[test]
    fn a_vanished_member_is_skipped_and_the_rest_are_reported() {
        let source = FakeProc::new().member(10, 10, 7).member(11, 11, 7);
        let members = session_members_with(&source, 7).unwrap();
        assert_eq!(
            members.iter().map(|member| member.pid).collect::<Vec<_>>(),
            vec![10, 11]
        );
        // A listed PID with no stat record is the ENOENT race, not an error.
        let mut vanished = FakeProc::new().member(10, 10, 7);
        vanished.pids.push(99);
        let members = session_members_with(&vanished, 7).unwrap();
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].pid, 10);
    }

    #[test]
    fn an_unreadable_identity_check_never_signals() {
        // Our own PID is a real process, so the pidfd opens; the injected stat
        // failure then has to stop the call *before* any signal is sent.
        let me = std::process::id();
        let source = FakeProc::new().member(me, me, me).unreadable(me);
        let member = SessionMember {
            pid: me,
            group: me,
            start: 0,
        };
        let error = signal_member_with(&source, me, &member).unwrap_err();
        assert!(
            format!("{error:#}").contains("injected permission failure"),
            "{error:#}"
        );
    }

    #[test]
    fn unverified_cleanup_keeps_the_armed_identity_and_does_not_reap() {
        let _serial = run_serial();
        let transport = spawn(&spec("printf 'READY\\n'; sleep 30")).unwrap();
        read_until(&transport, b"READY", BUDGET);
        let mut source = FakeProc::new();
        source.listing_fails = true;
        {
            let mut child = lock(&transport.child);
            let error = child.stop_with(&source).unwrap_err();
            assert!(
                format!("{error:#}").contains("injected /proc listing failure"),
                "{error:#}"
            );
            assert!(
                child.armed,
                "cleanup must stay armed when the session cannot be verified"
            );
            assert!(
                child.finished.is_none(),
                "an unverified cleanup must not report an exit status"
            );
        }
        // The real cleanup still owns the child afterwards.
        let mut fixture = SessionFixture::new(transport, None);
        fixture.stop().unwrap();
    }

    /// A stop that is requested but not yet confirmed must not finalize output:
    /// a live child can still write while cleanup runs, and only verified
    /// cleanup (or real terminal closure) may be reported as EOF.
    #[test]
    fn reads_stay_open_until_cleanup_is_confirmed() {
        let _serial = run_serial();
        let directory = tempfile::tempdir().unwrap();
        let gate = directory.path().join("gate");
        let transport = spawn(&spec(&format!(
            "stty raw -echo; printf 'READY\\n'; \
             while [ ! -e '{}' ]; do sleep 0.01; done; \
             printf 'TAIL-END\\n'; sleep 30",
            gate.display()
        )))
        .unwrap();
        read_until(&transport, b"READY", BUDGET);

        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        transport.install_stop_hook(Box::new(move || {
            let _ = entered_tx.send(());
            let _ = release_rx.recv();
        }));

        let mut fixture = SessionFixture::new(transport, Some(directory));
        std::thread::scope(|scope| {
            let transport = &fixture.transport;
            let handle = scope.spawn(move || transport.stop());
            entered_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("stop was never requested");

            // Stop is requested and cleanup is blocked: the child is alive and
            // writes its tail now.
            std::fs::write(&gate, b"go").unwrap();
            let deadline = Instant::now() + BUDGET;
            let mut collected = Vec::new();
            while !contains_bytes(&collected, b"TAIL-END") {
                assert!(Instant::now() < deadline, "the tail never arrived");
                let read = transport.read_bytes(4096, deadline, None).unwrap();
                assert!(!read.eof, "a live child was reported as EOF: {read:?}");
                collected.extend_from_slice(&read.bytes);
            }

            // Nothing queued right now, cleanup not confirmed: not final.
            let pending = loop {
                let read = transport
                    .read_bytes(4096, Instant::now() + Duration::from_secs(1), None)
                    .unwrap();
                if !read.bytes.is_empty() {
                    collected.extend_from_slice(&read.bytes);
                    continue;
                }
                break read;
            };
            assert!(
                !pending.eof,
                "pending cleanup must not report EOF: {pending:?}"
            );
            assert!(pending.cleanup_pending, "{pending:?}");

            release_tx.send(()).unwrap();
            handle.join().unwrap().unwrap();
        });

        // Cleanup is confirmed. An empty read is now final, and the tail is not
        // replayed by the drain.
        let mut rest = Vec::new();
        let final_read = loop {
            let read = fixture
                .transport
                .read_bytes(4096, Instant::now() + Duration::from_secs(5), None)
                .unwrap();
            rest.extend_from_slice(&read.bytes);
            if read.eof {
                break read;
            }
        };
        assert!(!final_read.cleanup_pending, "{final_read:?}");
        assert!(
            !contains_bytes(&rest, b"TAIL-END"),
            "the tail was replayed: {rest:?}"
        );
        fixture.stop().unwrap();
    }

    #[test]
    fn job_control_survivor_of_a_naturally_exited_leader_is_cleaned() {
        let _serial = run_serial();
        let transport = spawn(&SpawnSpec {
            executable: OsString::from("/bin/bash"),
            arguments: vec![
                OsString::from("-c"),
                OsString::from(
                    "set -m\n\
                     trap '' HUP\n\
                     (sleep 30) & printf 'SURVIVOR=%s\\n' \"$!\"\n\
                     printf 'READY\\n'\n\
                     exit 0\n",
                ),
            ],
            cwd: std::env::temp_dir(),
            environment: vec![(OsString::from("PATH"), OsString::from("/usr/bin:/bin"))],
            rows: 24,
            cols: 80,
        })
        .unwrap();
        let mut fixture = SessionFixture::new(transport, None);
        let transport = &fixture.transport;
        let output = read_until_markers(transport, &["SURVIVOR="], BUDGET);
        let leader = transport.process_id();
        let survivor = marker_value(&output, "SURVIVOR=").unwrap();
        println!("survivor fixture: leader={leader} survivor={survivor}");
        assert_eq!(session_of(survivor), Some(leader));
        assert_ne!(
            group_of_pid(survivor),
            Some(leader),
            "the survivor is not in a job-control group"
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

        fixture.stop().unwrap();
        wait_until_gone(survivor, Duration::from_secs(5));
        assert!(session_members(leader).unwrap().is_empty());
    }
}
