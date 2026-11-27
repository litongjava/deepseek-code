//! A real `dcl` process must cancel its own active work on SIGTERM/SIGHUP and
//! only then exit nonzero. The model fixture asks for a foreground Bash tool
//! or the Codex external-agent tool; both start a `sleep 30 & wait` subtree in
//! their own process group. Only the processes this test observes are checked
//! or stopped, never every process with a matching name.
#![cfg(unix)]

use axum::{
    extract::State,
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde_json::{json, Value};
use std::{
    os::unix::process::ExitStatusExt,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::process::{Child, Command};

#[derive(Clone)]
struct Fixture {
    requests: Arc<Mutex<Vec<Value>>>,
    tool: &'static str,
    arguments: Value,
}

async fn respond(State(state): State<Fixture>, Json(body): Json<Value>) -> Response {
    // Prompt-suggestion and other non-streaming calls only need a title.
    if body["stream"] != true {
        return Json(json!({
            "id": "summary", "object": "chat.completion", "created": 0, "model": "fixture-model",
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "title"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 8, "completion_tokens": 3, "total_tokens": 11}
        }))
        .into_response();
    }
    state.requests.lock().unwrap().push(body.clone());
    let last = body["messages"]
        .as_array()
        .and_then(|messages| messages.last());
    let delta = if last.is_some_and(|message| message["role"] != "tool") {
        json!({"tool_calls": [{"index": 0, "id": "call-fixture", "type": "function",
            "function": {"name": state.tool, "arguments": state.arguments.to_string()}}]})
    } else {
        json!({"content": "fixture finished"})
    };
    let finish = if last.is_some_and(|message| message["role"] != "tool") {
        "tool_calls"
    } else {
        "stop"
    };
    let chunks = [
        json!({"id": "fixture", "model": "fixture-model", "choices": [{"index": 0, "delta": delta, "finish_reason": null}]}),
        json!({"id": "fixture", "model": "fixture-model",
            "choices": [{"index": 0, "delta": {}, "finish_reason": finish}],
            "usage": {"prompt_tokens": 12, "completion_tokens": 8}}),
    ];
    let body = chunks
        .iter()
        .map(|chunk| format!("data: {chunk}\n\n"))
        .collect::<String>()
        + "data: [DONE]\n\n";
    ([("content-type", "text/event-stream")], body).into_response()
}

async fn server(tool: &'static str, arguments: Value) -> (String, tokio::task::JoinHandle<()>) {
    let fixture = Fixture {
        requests: Arc::new(Mutex::new(Vec::new())),
        tool,
        arguments,
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/chat/completions", post(respond))
        .with_state(fixture);
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (url, task)
}

/// A project and data directory with nothing inherited from the developer's
/// own dcl configuration.
fn workspace(temp: &Path) -> (PathBuf, PathBuf) {
    let data = temp.join("data");
    let project = temp.join("project");
    std::fs::create_dir_all(data.join("config")).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    (data, project)
}

fn command(data: &Path, project: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dcl"));
    command
        .args([
            "--data-dir",
            data.to_str().unwrap(),
            "--path",
            project.to_str().unwrap(),
            "--model",
            "fixture-model",
            "--api-format",
            "openai",
            "--api-key-env",
            "INTERRUPT_FIXTURE_KEY",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for key in [
        "DEEPSEEK_CODE_DATA_DIR",
        "DEEPSEEK_CODE_BACKGROUND_CONTROL",
        "DEEPSEEK_CODE_BACKGROUND_JOB",
        "DEEPSEEK_CODE_WORKER_MODEL",
        "DEEPSEEK_CODE_WORKER_AUTO_ROUTE",
        "DEEPSEEK_CODE_CHILD_API_KEY",
        // The fixture token belongs to one test run; a value inherited from the
        // caller would otherwise reach the fake agent and defeat cleanup.
        "DCL_SIGINT_TOKEN",
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "DEEPSEEK_API_KEY",
    ] {
        command.env_remove(key);
    }
    command.env("INTERRUPT_FIXTURE_KEY", "fixture-only-secret");
    command
}

fn alive(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

fn pid_from(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| text.trim().parse().ok())
}

async fn wait_for_pid(path: &Path) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Some(pid) = pid_from(path).filter(|pid| alive(*pid)) {
            return pid;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the fixture subtree never started: {}", path.display());
}

fn signal(pid: u32, name: &str) {
    let status = std::process::Command::new("kill")
        .args([name, &pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success(), "kill {name} {pid}");
}

/// Waits for the child and reports whether it ended by a signal.
async fn wait_for_exit(child: &mut Child) -> (ExitStatus, bool) {
    match tokio::time::timeout(Duration::from_secs(20), child.wait()).await {
        Ok(status) => {
            let status = status.unwrap();
            (status, status.signal().is_some())
        }
        Err(_) => {
            let _ = child.start_kill();
            let status = child.wait().await.unwrap();
            (status, false)
        }
    }
}

/// Polls until every observed process is gone; returns the ones still alive.
async fn survivors(pids: &[u32]) -> Vec<u32> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left: Vec<u32> = pids.iter().copied().filter(|pid| alive(*pid)).collect();
        if left.is_empty() || Instant::now() >= deadline {
            return left;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Best-effort cleanup so a failing assertion cannot leave the test subtree
/// running. Only the pids this test observed are signalled.
fn reap(pids: &[u32]) {
    for pid in pids {
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .status();
    }
}

fn sleep_tree_script(marker: &Path, parent_marker: Option<&Path>) -> String {
    let parent = parent_marker
        .map(|path| format!("echo $$ > '{}'\n", path.display()))
        .unwrap_or_default();
    format!(
        "#!/bin/sh\n{parent}sh -c 'sleep 30 & echo $! > \"{}\"; wait' &\nwait\n",
        marker.display()
    )
}

#[cfg(unix)]
fn write_executable(path: &Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, contents).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// The fake Codex startup fixture: a launcher that starts slowly, forks a
/// `sleep 120` child, and waits. Its processes are found and stopped through
/// Linux `pidfd` syscalls, whose numbers 434/424 are shared by x86_64 and
/// aarch64, so this compiles only for those 64-bit architectures. The pointer
/// width excludes the ILP32/x32 variants of those names.
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64"),
    target_pointer_width = "64"
))]
mod startup_agent {
    use super::*;
    use std::ffi::{c_int, c_long};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

    extern "C" {
        fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
        fn syscall(number: c_long, ...) -> c_long;
    }

    /// Requests the smallest pipe buffer Linux accepts, so one padded
    /// `stream-json` record cannot be written in a single pass. A host with
    /// larger pages may round this up; the test asserts on the value it reads
    /// back.
    pub const PIPE_BYTES: c_int = 4096;
    // F_SETPIPE_SZ / F_GETPIPE_SZ from asm-generic/fcntl.h.
    const F_SETPIPE_SZ: c_int = 1031;
    const F_GETPIPE_SZ: c_int = 1032;
    const SYS_PIDFD_OPEN: c_long = 434;
    const SYS_PIDFD_SEND_SIGNAL: c_long = 424;
    const SIGKILL: c_int = 9;
    const ESRCH: i32 = 3;
    const ENOSYS: i32 = 38;
    const EPERM: i32 = 1;

    /// Shrinks the pipe explicitly instead of assuming the host's default
    /// capacity is small enough to create backpressure.
    pub fn set_pipe_capacity(fd: RawFd, bytes: c_int) -> c_int {
        // SAFETY: fcntl with an integer size is the documented ABI here.
        unsafe { fcntl(fd, F_SETPIPE_SZ, bytes) }
    }

    pub fn pipe_capacity(fd: RawFd) -> c_int {
        // SAFETY: fcntl with no extra argument is the documented ABI here.
        unsafe { fcntl(fd, F_GETPIPE_SZ) }
    }

    /// A descriptor that names one process for its whole lifetime, not a
    /// numeric pid that may later belong to something else.
    fn pidfd_open(pid: i32) -> std::io::Result<OwnedFd> {
        // SAFETY: the syscall takes an integer pid and zero flags.
        let fd = unsafe { syscall(SYS_PIDFD_OPEN, pid as c_long, 0 as c_long) };
        if fd < 0 {
            // Read errno before anything else can overwrite it.
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: `fd` is a fresh descriptor this value now owns.
        Ok(unsafe { OwnedFd::from_raw_fd(fd as RawFd) })
    }

    fn pidfd_send(fd: &OwnedFd, signal: c_int) -> std::io::Result<()> {
        // SAFETY: the descriptor comes from `pidfd_open`; the rest is the signal
        // number, a null siginfo pointer and zero flags.
        let result = unsafe {
            syscall(
                SYS_PIDFD_SEND_SIGNAL,
                fd.as_raw_fd() as c_long,
                signal as c_long,
                0 as c_long,
                0 as c_long,
            )
        };
        if result < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    fn describe(operation: &str, pid: i32, error: &std::io::Error) -> String {
        let cause = match error.raw_os_error() {
            Some(ESRCH) => " (the process disappeared)",
            Some(ENOSYS) => " (pidfd syscalls are unavailable on this kernel)",
            Some(EPERM) => " (permission denied for this process)",
            _ => "",
        };
        format!("pidfd {operation} for pid {pid} failed{cause}: {error}")
    }

    /// Checks both syscalls against a process this test owns, before a
    /// long-lived fixture is started whose cleanup depends on them.
    pub fn preflight_pidfd() -> std::io::Result<()> {
        let own = pidfd_open(std::process::id() as i32)?;
        // Signal 0 probes the send syscall without delivering anything.
        pidfd_send(&own, 0)
    }

    /// A process's state letter: the first field after the parenthesized name.
    fn process_state(pid: i32) -> Option<String> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        stat.rsplit(')')
            .next()?
            .split_whitespace()
            .next()
            .map(str::to_owned)
    }

    /// A process that exited but has not been reaped yet owns no work, so it
    /// must not be reported as a survivor of a failed cleanup.
    fn process_is_zombie(pid: i32) -> bool {
        match process_state(pid) {
            Some(state) => state == "Z" || state == "X",
            // Unreadable means the entry is already gone.
            None => true,
        }
    }

    pub fn command_line(pid: i32) -> String {
        std::fs::read(format!("/proc/{pid}/cmdline"))
            .map(|bytes| {
                String::from_utf8_lossy(&bytes)
                    .replace('\0', " ")
                    .trim()
                    .to_owned()
            })
            .unwrap_or_default()
    }

    /// Remaining live processes and the diagnostics from a cleanup attempt.
    #[derive(Debug)]
    pub struct CleanupOutcome {
        pub remaining: Vec<i32>,
        pub errors: Vec<String>,
        /// True only when successful scans stayed empty for the whole settling
        /// window. An empty final scan alone is not proof: a process may have
        /// exited just before the deadline.
        pub settled: bool,
    }

    impl CleanupOutcome {
        pub fn is_clean(&self) -> bool {
            self.settled && self.errors.is_empty()
        }
    }

    /// True when the pid is gone or is a zombie, exactly as the discovery scan
    /// decides it. An orphaned process that exited owns no work even while a
    /// slow PID 1 has not reaped it.
    pub fn process_has_exited(pid: i32) -> bool {
        process_is_zombie(pid)
    }

    /// True once the descriptor's process is fully gone. `pidfd_send_signal`
    /// with signal 0 returns ESRCH then; while the task is still a zombie it
    /// returns success, which is why callers combine this with
    /// `process_has_exited`.
    pub fn pidfd_reports_gone(fd: &OwnedFd) -> bool {
        pidfd_send(fd, 0).err().and_then(|error| error.raw_os_error()) == Some(ESRCH)
    }

    /// Every fixture process of one test run, found by the token in its
    /// environment rather than by a pid the test happened to observe.
    /// Descendants inherit the token and the launcher re-execs with the exact
    /// expected value, so a stale inherited token cannot hide the tree.
    pub struct FixtureProcesses {
        token: String,
    }

    impl FixtureProcesses {
        pub fn new(token: String) -> Self {
            Self { token }
        }

        fn entry(&self) -> String {
            format!("DCL_SIGINT_TOKEN={}", self.token)
        }

        fn has_token(&self, pid: i32) -> bool {
            std::fs::read(format!("/proc/{pid}/environ")).is_ok_and(|environ| {
                environ
                    .split(|byte| *byte == 0)
                    .any(|item| item == self.entry().as_bytes())
            })
        }

        /// One discovery pass. Zombies own no work and are not reported; an
        /// unreadable proc root is an error, not an empty result.
        pub fn scan(&self) -> std::io::Result<Vec<i32>> {
            let entries = std::fs::read_dir("/proc")?;
            Ok(entries
                .flatten()
                .filter_map(|entry| entry.file_name().to_str()?.parse::<i32>().ok())
                .filter(|pid| !process_is_zombie(*pid) && self.has_token(*pid))
                .collect())
        }

        /// Signals one discovered pid through a pidfd. A pid reused after the
        /// scan fails the token recheck and is left alone; the descriptor is
        /// closed on every path.
        fn kill_pid(&self, pid: i32, errors: &mut Vec<String>) {
            let fd = match pidfd_open(pid) {
                Ok(fd) => fd,
                Err(error) if error.raw_os_error() == Some(ESRCH) => return,
                Err(error) => return push_error(errors, describe("open", pid, &error)),
            };
            if !self.has_token(pid) {
                return;
            }
            if let Err(error) = pidfd_send(&fd, SIGKILL) {
                if error.raw_os_error() != Some(ESRCH) {
                    push_error(errors, describe("send SIGKILL", pid, &error));
                }
            }
        }

        /// Repeated discovery and kill until the tagged set has stayed empty
        /// for 150 ms, or `budget` expires. `after_first_snapshot` runs once,
        /// after the first non-empty scan and before those processes are
        /// signalled, so a test can drive a controlled late fork.
        ///
        /// One deadline covers the whole call; the callback's own work is part
        /// of the caller's budget. Success requires the confirmed empty
        /// interval, so a process that exits just before the deadline cannot be
        /// reported as a clean result.
        ///
        /// A bounded scan cannot guarantee that an arbitrarily delayed process
        /// will be found; it closes the window for children forked while
        /// cleanup is running.
        pub fn settle_with(
            &self,
            budget: Duration,
            mut after_first_snapshot: impl FnMut(&[i32]),
        ) -> CleanupOutcome {
            let deadline = Instant::now() + budget;
            let mut errors = Vec::new();
            let mut released = false;
            let mut empty_since: Option<Instant> = None;
            let mut remaining: Vec<i32> = Vec::new();
            loop {
                match self.scan() {
                    Ok(pids) if pids.is_empty() => {
                        let since = *empty_since.get_or_insert_with(Instant::now);
                        if since.elapsed() >= Duration::from_millis(150) {
                            return CleanupOutcome {
                                remaining: Vec::new(),
                                errors,
                                settled: true,
                            };
                        }
                        remaining.clear();
                    }
                    Ok(pids) => {
                        empty_since = None;
                        if !released {
                            released = true;
                            after_first_snapshot(&pids);
                        }
                        for pid in &pids {
                            self.kill_pid(*pid, &mut errors);
                        }
                        remaining = pids;
                    }
                    Err(error) => {
                        empty_since = None;
                        push_error(&mut errors, format!("cannot enumerate /proc: {error}"));
                    }
                }
                if Instant::now() >= deadline {
                    push_error(
                        &mut errors,
                        "cleanup deadline expired before an empty interval was confirmed".into(),
                    );
                    return CleanupOutcome {
                        remaining,
                        errors,
                        settled: false,
                    };
                }
                std::thread::sleep(Duration::from_millis(25));
            }
        }

        pub fn settle(&self) -> CleanupOutcome {
            self.settle_with(Duration::from_secs(2), |_| {})
        }

        /// Opens a descriptor for the pid only while it still carries this
        /// run's token, so a pid reused by an unrelated process is never
        /// adopted. The descriptor pins identity for later checks.
        pub fn pidfd_if_tagged(&self, pid: i32) -> Option<OwnedFd> {
            let fd = pidfd_open(pid).ok()?;
            if self.has_token(pid) {
                Some(fd)
            } else {
                None
            }
        }

        /// Polls discovery until each named process appears with this run's
        /// token. Marker files alone do not prove the token, so this is the
        /// precondition before a survivor check means anything.
        pub fn require_discovered(&self, roles: &[(&str, u32)]) {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let discovered = self.scan().unwrap_or_default();
                let missing: Vec<String> = roles
                    .iter()
                    .filter(|(_, pid)| !discovered.contains(&(*pid as i32)))
                    .map(|(role, pid)| format!("{role} {pid}"))
                    .collect();
                if missing.is_empty() {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "fixture processes are not tagged with the run token after 5 s: {missing:?} (discovered: {discovered:?})"
                );
                std::thread::sleep(Duration::from_millis(25));
            }
        }
    }

    /// Diagnostics stay bounded: a persistent failure repeats on every pass.
    fn push_error(errors: &mut Vec<String>, message: String) {
        if errors.len() < 32 && !errors.contains(&message) {
            errors.push(message);
        }
    }

    impl Drop for FixtureProcesses {
        fn drop(&mut self) {
            // Cleanup may run while unwinding: it must not panic and must never
            // fall back to signalling a numeric pid. The message is built inside
            // the guard because formatting can panic on its own, and stderr is
            // written without a helper that could panic.
            let token = self.token.clone();
            let message = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let outcome = self.settle();
                if outcome.is_clean() {
                    None
                } else {
                    Some(format!(
                        "[interrupt_signals] fixture cleanup for {token} left live {outcome:?}\n"
                    ))
                }
            }))
            .unwrap_or_else(|_| {
                Some("[interrupt_signals] fixture cleanup panicked\n".to_string())
            });
            if let Some(message) = message {
                let _ = std::io::Write::write_all(&mut std::io::stderr(), message.as_bytes());
            }
        }
    }

    /// Reaps a directly spawned fixture process on every path.
    pub struct ReapedChild(pub Option<std::process::Child>);

    impl ReapedChild {
        pub fn spawn(command: &mut std::process::Command) -> Self {
            Self(Some(command.spawn().unwrap()))
        }
    }

    impl Drop for ReapedChild {
        fn drop(&mut self) {
            if let Some(mut child) = self.0.take() {
                if child.try_wait().ok().flatten().is_none() {
                    let _ = child.kill();
                }
                let _ = child.wait();
            }
        }
    }

    /// Waits for a marker file to name a live pid.
    pub fn wait_marker(path: &Path, secs: u64) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if let Some(pid) = pid_from(path).filter(|pid| alive(*pid)) {
                return pid;
            }
            assert!(
                Instant::now() < deadline,
                "the fixture marker never appeared: {}",
                path.display()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Writes its pid, starts slowly, then forks a `sleep 120` child that keeps
    /// the agent's stdout pipe open while the launcher waits for it.
    ///
    /// The first line replaces any inherited token with this run's exact value,
    /// so a stale value in the caller's environment cannot make the tree
    /// undiscoverable. `exec` keeps the pid, so the launcher stays one process.
    pub fn starting_agent_script(started: &Path, child: &Path, token: &str) -> String {
        format!(
            "#!/bin/sh\n\
             if [ \"${{DCL_SIGINT_TOKEN-}}\" != '{token}' ]; then exec env 'DCL_SIGINT_TOKEN={token}' \"$0\" \"$@\"; fi\n\
             echo $$ > '{started}'\n\
             sleep 0.5\n\
             sleep 120 &\n\
             echo $! > '{child}'\n\
             wait\n",
            started = started.display(),
            child = child.display(),
        )
    }

    /// Like `starting_agent_script`, but the fork only happens once the gate
    /// file exists, so a test can drive a fork after cleanup has scanned.
    pub fn gated_launcher_script(
        started: &Path,
        child: &Path,
        gate: &Path,
        token: &str,
    ) -> String {
        format!(
            "#!/bin/sh\n\
             if [ \"${{DCL_SIGINT_TOKEN-}}\" != '{token}' ]; then exec env 'DCL_SIGINT_TOKEN={token}' \"$0\" \"$@\"; fi\n\
             echo $$ > '{started}'\n\
             while [ ! -e '{gate}' ]; do sleep 0.01; done\n\
             sleep 120 &\n\
             echo $! > '{child}'\n\
             wait\n",
            started = started.display(),
            child = child.display(),
            gate = gate.display(),
        )
    }

    /// Reads the CLI's stdout and stops when the large tool_start record starts,
    /// exactly while the event is being handled and no interrupt waiter exists.
    /// It resumes on `release` and then drains to EOF, so the CLI can finish.
    pub fn gated_output(
        mut output: tokio::process::ChildStdout,
        blocked: tokio::sync::oneshot::Sender<()>,
        mut release: tokio::sync::oneshot::Receiver<()>,
    ) -> tokio::task::JoinHandle<()> {
        use tokio::io::AsyncReadExt;
        tokio::spawn(async move {
            let mut blocked = Some(blocked);
            let mut byte = [0u8; 1];
            let mut line = Vec::new();
            let mut paused = false;
            loop {
                match output.read(&mut byte).await {
                    Ok(0) => break,
                    Ok(_) if !paused => {
                        if byte[0] == b'\n' {
                            line.clear();
                        } else {
                            line.push(byte[0]);
                            // serde_json orders object keys, and "display_title"
                            // sorts first in the tool_start record.
                            if line.starts_with(b"{\"display_title\"") {
                                paused = true;
                                if let Some(blocked) = blocked.take() {
                                    let _ = blocked.send(());
                                }
                                let _ = (&mut release).await;
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        })
    }
}

async fn interrupt_foreground_bash(signal_name: &str) {
    let temp = tempfile::tempdir().unwrap();
    let (data, project) = workspace(temp.path());
    let marker = temp.path().join("bash-child.pid");
    let arguments = json!({
        "command": format!("sh -c 'sleep 30 & echo $! > \"{}\"; wait'", marker.display()),
        "description": "Hold a foreground shell open"
    });
    let (url, server) = server("Bash", arguments).await;
    let mut child = command(&data, &project)
        .args([
            "--base-url",
            &url,
            "--permission-mode",
            "bypass",
            "query",
            "-p",
            "hold the shell open",
        ])
        .spawn()
        .unwrap();
    let dcl = child.id().unwrap();
    let sleep_pid = wait_for_pid(&marker).await;
    assert!(alive(dcl), "dcl exited before the signal");
    signal(dcl, signal_name);
    let (status, by_signal) = wait_for_exit(&mut child).await;
    let left = survivors(&[sleep_pid]).await;
    reap(&[sleep_pid, dcl]);
    server.abort();
    assert!(
        !status.success(),
        "{signal_name}: the interrupted turn must not report success"
    );
    assert!(
        !by_signal,
        "{signal_name}: dcl must cancel the turn and exit on its own, not die from the signal (survivors after the wait: {left:?})"
    );
    assert!(
        left.is_empty(),
        "{signal_name}: the foreground Bash subtree survived: {left:?}"
    );
}

#[tokio::test]
async fn sigterm_cancels_a_foreground_bash_subtree_then_exits_nonzero() {
    interrupt_foreground_bash("-TERM").await;
}

#[tokio::test]
async fn sighup_cancels_a_foreground_bash_subtree_then_exits_nonzero() {
    interrupt_foreground_bash("-HUP").await;
}

#[tokio::test]
async fn sigterm_cancels_the_codex_agent_tree_then_exits_nonzero() {
    let temp = tempfile::tempdir().unwrap();
    let (data, project) = workspace(temp.path());
    // Offer the Codex tool: enabled and resident, with a controlled executable
    // first on PATH. The script never emits lifecycle events; it only starts a
    // process tree, so the measurement is the cancellation, not the protocol.
    deepseek_code_core::data_layout::write_config_file(
        data.join("config/customization.json"),
        json!({"tool_loading": {"Codex": {"enabled": true, "on_demand": false}}}).to_string(),
    )
    .unwrap();
    let bin = temp.path().join("fake-bin");
    std::fs::create_dir(&bin).unwrap();
    let sleep_marker = temp.path().join("codex-child.pid");
    let codex_marker = temp.path().join("codex-parent.pid");
    write_executable(
        &bin.join("codex"),
        &sleep_tree_script(&sleep_marker, Some(&codex_marker)),
    );
    let (url, server) = server("Codex", json!({"prompt": "hold the agent open"})).await;
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut child = command(&data, &project)
        .env("PATH", path)
        .args([
            "--base-url",
            &url,
            "--permission-mode",
            "bypass",
            "query",
            "-p",
            "delegate the work",
        ])
        .spawn()
        .unwrap();
    let dcl = child.id().unwrap();
    let sleep_pid = wait_for_pid(&sleep_marker).await;
    let codex_pid = wait_for_pid(&codex_marker).await;
    assert!(alive(dcl), "dcl exited before the signal");
    signal(dcl, "-TERM");
    let (status, by_signal) = wait_for_exit(&mut child).await;
    let left = survivors(&[sleep_pid, codex_pid]).await;
    reap(&[sleep_pid, codex_pid, dcl]);
    server.abort();
    assert!(
        !status.success(),
        "the interrupted turn must not report success"
    );
    assert!(
        !by_signal,
        "dcl must cancel the turn and exit on its own, not die from the signal (survivors after the wait: {left:?})"
    );
    assert!(left.is_empty(), "the Codex tool subtree survived: {left:?}");
}

/// One SIGINT must cancel a turn whose Codex launcher is still starting. The
/// candidate signal arrives while the CLI is synchronously writing the large
/// tool_start record, after the previous interrupt wait was dropped. A fresh
/// wait used to take the already-incremented counter as its baseline and the
/// turn kept running; the turn-scoped observer remembers the original count.
///
/// Linux only, on the two architectures whose pidfd syscall numbers are known:
/// it reads `/proc` for process identity and cleanup, and it keeps the padded
/// prompt below Linux's single-argument limit.
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64"),
    target_pointer_width = "64"
))]
#[tokio::test]
async fn sigint_while_an_agent_is_starting_cancels_the_turn() {
    use startup_agent::*;
    use std::os::fd::AsRawFd;

    preflight_pidfd().unwrap_or_else(|error| {
        panic!("pidfd cleanup is unavailable, so this fixture could not be verified: {error}")
    });
    let temp = tempfile::tempdir().unwrap();
    let (data, project) = workspace(temp.path());
    deepseek_code_core::data_layout::write_config_file(
        data.join("config/customization.json"),
        json!({"tool_loading": {"Codex": {"enabled": true, "on_demand": false}}}).to_string(),
    )
    .unwrap();
    let bin = temp.path().join("fake-bin");
    std::fs::create_dir(&bin).unwrap();
    let started_marker = temp.path().join("codex-started.pid");
    let child_marker = temp.path().join("codex-child.pid");
    let token = format!(
        "dcl-sigint-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    // Created before dcl starts, so an early return or panic still kills every
    // fixture process that exists by then.
    let fixtures = FixtureProcesses::new(token.clone());
    write_executable(
        &bin.join("codex"),
        &starting_agent_script(&started_marker, &child_marker, &token),
    );
    // The padded prompt stays under Linux's 128 KiB per-argument limit because
    // the Codex tool passes it as one argv entry, and it is far larger than the
    // pipe capacity enforced below.
    let padded = format!("hold the agent open {}", "X".repeat(110_000));
    let (url, server) = server("Codex", json!({ "prompt": padded.clone() })).await;
    let (blocked_tx, blocked_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let stderr = std::fs::File::create(temp.path().join("dcl-stderr.log")).unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let mut child = command(&data, &project)
        .env("PATH", path)
        .stdout(Stdio::piped())
        .stderr(Stdio::from(stderr))
        .args([
            "--base-url",
            &url,
            "--permission-mode",
            "bypass",
            "--output-format",
            "stream-json",
            "query",
            "-p",
            "delegate the work",
        ])
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let fd = stdout.as_raw_fd();
    let set = set_pipe_capacity(fd, PIPE_BYTES);
    assert!(
        set > 0,
        "F_SETPIPE_SZ failed: {}",
        std::io::Error::last_os_error()
    );
    let capacity = pipe_capacity(fd);
    assert!(
        capacity > 0 && capacity <= 65536,
        "the stdout pipe capacity is {capacity}, not a small buffer"
    );
    assert!(
        (capacity as usize) < padded.len(),
        "a {capacity}-byte pipe can hold the whole {}-byte record",
        padded.len()
    );
    let reader = gated_output(stdout, blocked_tx, release_rx);
    let dcl = child.id().unwrap();
    tokio::time::timeout(Duration::from_secs(20), blocked_rx)
        .await
        .expect("the CLI never blocked on the tool_start record")
        .expect("the stdout gate closed before the record");
    let launcher_pid = wait_for_pid(&started_marker).await;
    // The forked child must exist too, so the cleanup assertion has a real
    // descendant to prove and the agent keeps its stdout open.
    let child_pid = wait_for_pid(&child_marker).await;
    // A marker proves the process started, not that it carries this run's
    // token; without this the survivor check below could pass vacuously.
    fixtures.require_discovered(&[("launcher", launcher_pid), ("child", child_pid)]);
    assert!(alive(dcl), "dcl exited before the signal");
    // Other runtime workers publish the signal while the main task is blocked
    // in the synchronous write.
    tokio::time::sleep(Duration::from_millis(200)).await;
    signal(dcl, "-INT");
    tokio::time::sleep(Duration::from_millis(200)).await;
    release_tx.send(()).unwrap();
    let exit = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;
    let (status, by_signal) = match exit {
        Ok(status) => {
            let status = status.unwrap();
            (Some(status), status.signal().is_some())
        }
        Err(_) => (None, false),
    };
    if status.is_none() {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
    server.abort();
    let _ = reader.await;
    // This assertion runs before any guard cleanup: the fixture tree must be
    // gone because dcl cancelled it, not because this test killed it. Zombies
    // own no work and are not counted, and a short settling window covers the
    // kernel's own reaping.
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut left = Vec::new();
    let mut found_empty = false;
    let mut last_error = None;
    loop {
        match fixtures.scan() {
            Ok(pids) if pids.is_empty() => {
                found_empty = true;
                left.clear();
                break;
            }
            Ok(pids) => {
                left = pids;
                last_error = None;
            }
            Err(error) => last_error = Some(error.to_string()),
        }
        if Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let details: Vec<String> = left
        .iter()
        .map(|pid| format!("{pid}: {}", command_line(*pid)))
        .collect();
    assert!(
        status.is_some(),
        "dcl did not exit within 10 s of one SIGINT (survivors: {details:?})"
    );
    let status = status.unwrap();
    assert!(
        !status.success(),
        "the interrupted turn must not report success"
    );
    assert!(
        !by_signal,
        "dcl must cancel the turn and exit on its own, not die from the signal"
    );
    assert!(
        found_empty,
        "the starting Codex subtree survived (survivors: {details:?}, scan error: {last_error:?})"
    );
}

/// Cleanup must find a descendant forked after its first snapshot: the driver
/// here releases a gated launcher from inside the first snapshot, so the child
/// exists only on a later pass.
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64"),
    target_pointer_width = "64"
))]
#[test]
fn cleanup_rescans_for_a_child_forked_after_discovery() {
    use startup_agent::*;
    preflight_pidfd().unwrap_or_else(|error| {
        panic!("pidfd cleanup is unavailable, so this fixture could not be verified: {error}")
    });
    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("codex");
    let started = temp.path().join("launcher.pid");
    let child_marker = temp.path().join("child.pid");
    let gate = temp.path().join("gate");
    let token = format!(
        "dcl-sigint-cleanup-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    write_executable(
        &script,
        &gated_launcher_script(&started, &child_marker, &gate, &token),
    );
    let fixtures = FixtureProcesses::new(token.clone());
    let mut command = std::process::Command::new(&script);
    command
        .env("DCL_SIGINT_TOKEN", &token)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let launcher = ReapedChild::spawn(&mut command);
    let launcher_pid = wait_marker(&started, 10);
    fixtures.require_discovered(&[("launcher", launcher_pid)]);
    // The callback runs between the first snapshot and the kills it authorises.
    let mut late_child = None;
    let mut late_child_fd = None;
    let outcome = fixtures.settle_with(Duration::from_secs(7), |snapshot| {
        assert!(
            snapshot.contains(&(launcher_pid as i32)),
            "the first snapshot missed the launcher: {snapshot:?}"
        );
        std::fs::write(&gate, b"").unwrap();
        let child = wait_marker(&child_marker, 10);
        // Pin identity while the child is known and alive: the descriptor makes
        // the later check immune to pid reuse.
        late_child_fd = Some(
            fixtures
                .pidfd_if_tagged(child as i32)
                .expect("the late child did not carry this run's token"),
        );
        late_child = Some(child);
    });
    let late_child = late_child.expect("the gated fork never happened");
    let late_child_fd = late_child_fd.expect("the late child was never adopted");
    assert!(
        outcome.is_clean(),
        "cleanup did not settle: {outcome:?}"
    );
    // A killed orphan can stay a zombie while a slow PID 1 has not reaped it,
    // which owns no work: accept either the descriptor reporting the process
    // gone or the same zombie-aware predicate the discovery scan uses.
    let deadline = Instant::now() + Duration::from_secs(2);
    while !(pidfd_reports_gone(&late_child_fd) || process_has_exited(late_child as i32)) {
        assert!(
            Instant::now() < deadline,
            "the child forked after the first snapshot is still running (pid {late_child})"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    drop(launcher);
}

/// A final empty scan that the deadline cut short is not a confirmed cleanup.
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64"),
    target_pointer_width = "64"
))]
#[test]
fn an_unconfirmed_empty_scan_is_not_clean() {
    use startup_agent::CleanupOutcome;
    let outcome = CleanupOutcome {
        remaining: Vec::new(),
        errors: Vec::new(),
        settled: false,
    };
    assert!(
        !outcome.is_clean(),
        "an empty final scan without the settling interval was reported as clean"
    );
}

/// The launcher replaces a wrong inherited token, so neither the survivor check
/// nor the cleanup can be satisfied by a stale value in the caller's
/// environment.
#[cfg(all(
    target_os = "linux",
    any(target_arch = "x86_64", target_arch = "aarch64"),
    target_pointer_width = "64"
))]
#[test]
fn the_launcher_replaces_a_wrong_inherited_token() {
    use startup_agent::*;
    preflight_pidfd().unwrap_or_else(|error| {
        panic!("pidfd cleanup is unavailable, so this fixture could not be verified: {error}")
    });
    let temp = tempfile::tempdir().unwrap();
    let started = temp.path().join("launcher.pid");
    let child_marker = temp.path().join("child.pid");
    let script = temp.path().join("codex");
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let token = format!(
        "dcl-sigint-reexec-{}-{}",
        std::process::id(),
        nanos
    );
    // Unique per run. If the launcher's re-exec regressed, the whole tree keeps
    // this value, so it needs its own guard: the expected-token guard alone
    // would never find it.
    let stale_token = format!("dcl-sigint-stale-{}-{}", std::process::id(), nanos);
    let stale_fixtures = FixtureProcesses::new(stale_token.clone());
    assert!(stale_fixtures.scan().is_ok());
    write_executable(
        &script,
        &starting_agent_script(&started, &child_marker, &token),
    );
    let fixtures = FixtureProcesses::new(token.clone());
    let mut command = std::process::Command::new(&script);
    command
        .env("DCL_SIGINT_TOKEN", &stale_token)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let launcher = ReapedChild::spawn(&mut command);
    let launcher_pid = wait_marker(&started, 10);
    let child_pid = wait_marker(&child_marker, 10);
    fixtures.require_discovered(&[("launcher", launcher_pid), ("child", child_pid)]);
    drop(launcher);
    let outcome = fixtures.settle();
    assert!(
        outcome.is_clean(),
        "cleanup of the re-executed fixture did not settle: {outcome:?}"
    );
    // Nothing in the tree should still carry the stale value.
    assert!(
        stale_fixtures.scan().unwrap_or_default().is_empty(),
        "a process kept the stale inherited token"
    );
}
