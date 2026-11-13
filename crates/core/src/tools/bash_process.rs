//! Bounded shell output and descendant lifetime management.
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};
const OUTPUT_LIMIT: usize = 1024 * 1024;

/// bash reports a stage killed by SIGPIPE as 128 + 13. With `-o pipefail` that
/// only reaches us when a reader such as `head` closed the pipe early, and it is
/// the same number for Git Bash on Windows; other shells do not use it.
fn cut_short_by_reader(shell: &str, code: Option<i32>) -> bool {
    code == Some(128 + 13) && matches!(shell, "bash" | "zsh" | "sh" | "wsl")
}

struct Captured {
    head: Vec<u8>,
    tail: std::collections::VecDeque<u8>,
    total: usize,
    /// Bytes kept before the middle is dropped. A caller that must parse a
    /// complete document can raise this; Bash keeps the historical value.
    limit: usize,
}
impl Default for Captured {
    fn default() -> Self {
        Self {
            head: Vec::new(),
            tail: std::collections::VecDeque::new(),
            total: 0,
            limit: OUTPUT_LIMIT,
        }
    }
}
impl Captured {
    fn with_limit(limit: usize) -> Self {
        Self {
            limit,
            ..Self::default()
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        self.total = self.total.saturating_add(bytes.len());
        let head = bytes
            .len()
            .min((self.limit / 2).saturating_sub(self.head.len()));
        self.head.extend_from_slice(&bytes[..head]);
        let rest = &bytes[head..];
        let remove = (self.tail.len() + rest.len()).saturating_sub(self.limit / 2);
        self.tail.drain(..remove.min(self.tail.len()));
        self.tail.extend(rest.iter().copied());
    }

    /// Whether more bytes arrived than this capture keeps.
    fn overflowed(&self) -> bool {
        self.total > self.limit
    }

    fn text(&self) -> String {
        let mut bytes = self.head.clone();
        let tail: Vec<_> = self.tail.iter().copied().collect();
        let mut tail_start = 0;
        if self.overflowed() {
            if let Err(error) = std::str::from_utf8(&bytes) {
                if error.error_len().is_none() {
                    bytes.truncate(error.valid_up_to());
                }
            }
            while tail_start < tail.len() && tail[tail_start] & 0xc0 == 0x80 {
                tail_start += 1;
            }
            bytes.extend_from_slice(b"\n... (capture truncated; middle omitted) ...\n");
        }
        bytes.extend_from_slice(&tail[tail_start..]);
        String::from_utf8_lossy(&bytes).into_owned()
    }

    /// Bash presentation: the capture overflowed, or the head/tail preview is
    /// still larger than the configured tool output limit.
    fn truncated(&self) -> bool {
        self.overflowed() || self.text().len() > super::max_output_bytes()
    }
}

async fn capture(
    mut pipe: impl AsyncRead + Unpin,
    output: &mut Captured,
    log: Option<std::path::PathBuf>,
) -> std::io::Result<()> {
    let mut log = log.map(std::fs::File::create).transpose()?;
    let mut buffer = [0u8; 8192];
    loop {
        let count = pipe.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        let keep = count.min(OUTPUT_LIMIT.saturating_sub(output.total));
        output.push(&buffer[..count]);
        if let Some(log) = log.as_mut() {
            std::io::Write::write_all(log, &buffer[..keep])?;
        }
    }
    Ok(())
}

pub(super) async fn run(command: &mut Command, timeout: Duration, shell: &str) -> Result<Value> {
    run_logged(command, Some(timeout), shell, None, None).await
}

/// `stdin` is `Some` only for an explicit interactive pipe; every ordinary call
/// keeps the historical null stdin.
pub(super) async fn run_logged(
    command: &mut Command,
    timeout: Option<Duration>,
    shell: &str,
    logs: Option<(std::path::PathBuf, std::path::PathBuf)>,
    stdin: Option<std::fs::File>,
) -> Result<Value> {
    let run = run_captured(command, timeout, logs, stdin, OUTPUT_LIMIT).await?;
    if run.timed_out {
        let timeout = timeout.unwrap_or_default();
        return Ok(
            json!({"error":format!("Command timed out after {} ms; descendants were terminated. For long installations or builds, set timeout explicitly (milliseconds, at most 600000), or use run_in_background.",timeout.as_millis()),"is_error":true,"timed_out":true,"shell":shell,"stdout":super::limit_output(&run.stdout.text()),"stderr":super::limit_output(&run.stderr.text()),"stdout_truncated":run.stdout.truncated(),"stderr_truncated":run.stderr.truncated()}),
        );
    }
    let status = run.status.expect("a finished capture has a status");
    let code = status.code();
    // A pipeline cut short by `head` still produced the output the caller
    // asked for. The exit code and the flag keep it visible.
    let cut_short = cut_short_by_reader(shell, code);
    Ok(json!({
        "stdout":super::limit_output(&run.stdout.text()),
        "stderr":super::limit_output(&run.stderr.text()),
        "stdout_truncated":run.stdout.truncated(),"stderr_truncated":run.stderr.truncated(),
        "exit_code":code.unwrap_or(-1),
        "success":status.success() || cut_short,
        "pipeline_sigpipe":cut_short,
        "shell":shell
    }))
}

/// Runs a command with a bounded **raw** capture: the whole document within
/// `limit` is kept and returned as it was, with no head/tail presentation
/// preview, so a caller can parse complete JSON inside that explicit cap.
/// Anything larger is reported as overflow instead of being handed on as a
/// silently mangled document.
pub(super) async fn run_raw(
    command: &mut Command,
    timeout: Duration,
    limit: usize,
    stdin: Option<std::fs::File>,
) -> Result<Value> {
    let run = run_captured(command, Some(timeout), None, stdin, limit).await?;
    let code = run.status.and_then(|status| status.code());
    Ok(json!({
        "stdout":run.stdout.text(),
        "stderr":run.stderr.text(),
        "stdout_truncated":run.stdout.overflowed(),
        "stderr_truncated":run.stderr.overflowed(),
        "exit_code":code.unwrap_or(-1),
        "success":run.status.is_some_and(|status| status.success()),
        "timed_out":run.timed_out
    }))
}

/// One bounded capture of a child process.
struct CapturedRun {
    status: Option<std::process::ExitStatus>,
    timed_out: bool,
    stdout: Captured,
    stderr: Captured,
}

async fn run_captured(
    command: &mut Command,
    timeout: Option<Duration>,
    logs: Option<(std::path::PathBuf, std::path::PathBuf)>,
    stdin: Option<std::fs::File>,
    limit: usize,
) -> Result<CapturedRun> {
    command
        .kill_on_drop(true)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    match stdin {
        Some(stdin) => command.stdin(Stdio::from(stdin)),
        None => command.stdin(Stdio::null()),
    };
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().context("Starting shell command")?;
    let tree = crate::services::tool_hooks::ProcessTree::new(
        child.id().context("Shell process ID missing")?,
    )?;
    let stdout = child.stdout.take().context("Shell stdout missing")?;
    let stderr = child.stderr.take().context("Shell stderr missing")?;
    let (mut stdout_bytes, mut stderr_bytes) =
        (Captured::with_limit(limit), Captured::with_limit(limit));
    let (out_log, err_log) = logs
        .map(|(out, err)| (Some(out), Some(err)))
        .unwrap_or_default();
    let execution = async {
        // Reap the root before waiting for EOF: descendants may inherit the pipes.
        let wait = async {
            let status = child.wait().await;
            drop(tree);
            status
        };
        let (status, out, err) = tokio::join!(
            wait,
            capture(stdout, &mut stdout_bytes, out_log),
            capture(stderr, &mut stderr_bytes, err_log)
        );
        out?;
        err?;
        status
    };
    let result = match timeout {
        Some(timeout) => tokio::time::timeout(timeout, execution).await,
        None => Ok(execution.await),
    };
    match result {
        Ok(Ok(status)) => Ok(CapturedRun {
            status: Some(status),
            timed_out: false,
            stdout: stdout_bytes,
            stderr: stderr_bytes,
        }),
        Ok(Err(error)) => Err(error).context("Reading shell command output"),
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Ok(CapturedRun {
                status: None,
                timed_out: true,
                stdout: stdout_bytes,
                stderr: stderr_bytes,
            })
        }
    }
}

/// Starts a long-lived service that must outlive this call. It is deliberately
/// outside the Job Object that `run_logged` relies on: the trade for surviving
/// our exit is that it also survives cancellation, so the caller owns cleanup.
pub(super) fn start_detached(mut command: Command, directory: &Path, shell: &str) -> Result<Value> {
    std::fs::create_dir_all(directory)?;
    let stdout = directory.join("stdout.log");
    let stderr = directory.join("stderr.log");

    #[cfg(windows)]
    let (pid, breakaway) = {
        configure_detached(&mut command, &stdout, &stderr)?;
        command.creation_flags(detached_flags(true));
        match command.spawn() {
            Ok(child) => (child.id().unwrap_or(0), true),
            // A Job without JOB_OBJECT_LIMIT_BREAKAWAY_OK refuses the flag. Start
            // anyway and report the weaker guarantee rather than failing outright.
            Err(error) if error.raw_os_error() == Some(5) => {
                configure_detached(&mut command, &stdout, &stderr)?;
                command.creation_flags(detached_flags(false));
                let child = command.spawn().context("Starting the detached command")?;
                (child.id().unwrap_or(0), false)
            }
            Err(error) => return Err(error).context("Starting the detached command"),
        }
    };

    #[cfg(unix)]
    let (pid, breakaway) = {
        configure_detached(&mut command, &stdout, &stderr)?;
        unsafe {
            command.pre_exec(|| {
                // Its own session, so closing our terminal cannot signal it.
                rustix::process::setsid()
                    .map_err(|error| std::io::Error::from_raw_os_error(error.raw_os_error()))?;
                Ok(())
            });
        }
        let child = command.spawn().context("Starting the detached command")?;
        (child.id().unwrap_or(0), true)
    };

    #[cfg(not(any(windows, unix)))]
    let (pid, breakaway) = {
        configure_detached(&mut command, &stdout, &stderr)?;
        let child = command.spawn().context("Starting the detached command")?;
        (child.id().unwrap_or(0), false)
    };

    // Only a real breakaway starts outside the caller's Job Object. Saying the
    // service survives when it does not would be worse than not starting it.
    let message = if breakaway {
        "Started as an unmanaged service: it outlives this call and is not tracked as a task. Stop it explicitly when it is no longer needed."
    } else {
        "Started, but the host refused to detach it from its process group (the enclosing Job Object does not allow breakaway). It may be terminated together with this call's process tree; run it from a terminal outside that Job Object if it must keep running."
    };
    Ok(json!({
        "detached": true,
        "pid": pid,
        "stdout_file": stdout,
        "stderr_file": stderr,
        "isolated": breakaway,
        "shell": shell,
        "message": message
    }))
}

fn configure_detached(command: &mut Command, stdout: &Path, stderr: &Path) -> Result<()> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(std::fs::File::create(stdout)?))
        .stderr(Stdio::from(std::fs::File::create(stderr)?))
        // Dropping the handle must not kill the service.
        .kill_on_drop(false);
    Ok(())
}

#[cfg(windows)]
fn detached_flags(breakaway: bool) -> u32 {
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    DETACHED_PROCESS
        | CREATE_NEW_PROCESS_GROUP
        | if breakaway {
            CREATE_BREAKAWAY_FROM_JOB
        } else {
            0
        }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn root_exit_does_not_wait_for_inherited_pipes() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let mut command = Command::new("node");
        command.args(["-e", "require('child_process').spawn(process.execPath,['-e','setInterval(()=>{},10000)'],{stdio:'inherit'}).unref();console.log('root done');setTimeout(()=>process.exit(0),200)"]);
        let started = std::time::Instant::now();
        let result = run(&mut command, Duration::from_secs(8), "node-fixture")
            .await
            .unwrap();
        assert_eq!(result["success"], true, "{result}");
        assert!(result["stdout"].as_str().unwrap().contains("root done"));
        assert!(started.elapsed() < Duration::from_secs(5));
    }
    #[tokio::test]
    async fn output_is_bounded_while_both_pipes_are_drained() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let mut command = Command::new("node");
        command.args(["-e", "process.stdout.write('START\\n'+'x'.repeat(2*1024*1024)+'\\nTEST SUMMARY');process.stderr.write('ERROR START\\n'+'y'.repeat(2*1024*1024)+'\\nFINAL ERROR')"]);
        let result = run(&mut command, Duration::from_secs(10), "node-fixture")
            .await
            .unwrap();
        assert_eq!(result["success"], true);
        assert_eq!(result["stdout_truncated"], true);
        assert_eq!(result["stderr_truncated"], true);
        assert!(result["stdout"].as_str().unwrap().len() <= OUTPUT_LIMIT + 200);
        assert!(result["stdout"].as_str().unwrap().starts_with("START"));
        assert!(result["stdout"].as_str().unwrap().ends_with("TEST SUMMARY"));
        assert!(result["stderr"].as_str().unwrap().ends_with("FINAL ERROR"));
    }

    #[tokio::test]
    async fn tool_limit_sets_flags_before_capture_limit() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let mut command = Command::new("node");
        command.args(["-e", "process.stdout.write('x'.repeat(200*1024)+'END')"]);
        let result = run(&mut command, Duration::from_secs(10), "node-fixture")
            .await
            .unwrap();
        assert_eq!(result["stdout_truncated"], true);
        assert_eq!(result["stderr_truncated"], false);
        assert!(result["stdout"].as_str().unwrap().ends_with("END"));
    }
    #[tokio::test]
    async fn timeout_retains_partial_diagnostics() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let mut command = Command::new("node");
        command.args(["-e", "process.stdout.write('install started');process.stderr.write('registry waiting');setInterval(()=>{},10000)"]);
        let result = run(&mut command, Duration::from_millis(700), "node-fixture")
            .await
            .unwrap();
        assert_eq!(result["timed_out"], true);
        assert_eq!(result["is_error"], true);
        assert_eq!(result["stdout"], "install started");
        assert_eq!(result["stderr"], "registry waiting");
        assert_eq!(result["stdout_truncated"], false);
    }

    async fn descendants(cancel: bool) {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let ready = temp.path().join("ready");
        let leaked = temp.path().join("leaked");
        let code = r#"const cp=require('child_process'),fs=require('fs');cp.spawn(process.execPath,['-e',`setTimeout(()=>require('fs').writeFileSync(process.argv[1],'leaked'),1500)`,process.argv[2]],{stdio:'inherit'});fs.writeFileSync(process.argv[1],'ready');setInterval(()=>{},10000)"#;
        let mut command = Command::new("node");
        command.arg("-e").arg(code).arg(&ready).arg(&leaked);
        let task = tokio::spawn(async move {
            run(
                &mut command,
                Duration::from_millis(if cancel { 10000 } else { 700 }),
                "node-fixture",
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(5), async {
            while !ready.exists() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        if cancel {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        } else {
            assert_eq!(task.await.unwrap().unwrap()["timed_out"], true);
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(
            !leaked.exists(),
            "Grandchild survived shell cancellation/timeout"
        );
    }
    #[tokio::test]
    async fn timeout_terminates_grandchildren() {
        descendants(false).await;
    }
    #[tokio::test]
    async fn cancellation_terminates_grandchildren() {
        descendants(true).await;
    }

    #[test]
    fn sigpipe_is_recognized_only_for_bash_like_shells() {
        assert!(cut_short_by_reader("bash", Some(141)));
        assert!(cut_short_by_reader("zsh", Some(141)));
        assert!(cut_short_by_reader("sh", Some(141)));
        assert!(!cut_short_by_reader("powershell", Some(141)));
        assert!(!cut_short_by_reader("cmd", Some(141)));
        assert!(!cut_short_by_reader("bash", Some(1)));
        assert!(!cut_short_by_reader("bash", None));
    }

    /// A detached start must hand back a running process whose output is in the
    /// log files, and must not claim a stronger lifetime guarantee than it got.
    #[tokio::test]
    async fn detached_start_reports_a_live_service_and_its_real_isolation() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let temp = tempfile::tempdir().unwrap();
        let marker = temp.path().join("alive");
        let mut command = Command::new("node");
        command.arg("-e").arg(format!(
            "console.log('started');setTimeout(()=>require('fs').writeFileSync({:?},'alive'),600);setInterval(()=>{{}},10000)",
            marker.to_string_lossy()
        ));
        let result = start_detached(command, temp.path(), "node-fixture").unwrap();
        let pid = result["pid"].as_u64().unwrap();
        assert_eq!(result["detached"], true);
        assert!(pid > 0, "{result}");

        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(
            marker.exists(),
            "Detached service did not keep running: {result}"
        );
        let log = std::fs::read_to_string(temp.path().join("stdout.log")).unwrap();
        assert!(
            log.contains("started"),
            "Detached stdout was not captured: {log:?}"
        );
        let isolated = result["isolated"].as_bool().unwrap();
        let message = result["message"].as_str().unwrap();
        assert_eq!(message.contains("outlives this call"), isolated, "{result}");
        stop_test_service(pid as u32);
    }

    /// procps `kill -9 -123456` without `--` signals pid -1 (every process of the
    /// user), so an external `kill` given a negative pid must separate it with `--`.
    #[test]
    fn external_kill_never_takes_a_negative_pid_without_separator() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut offenders = Vec::new();
        for dir in ["crates", "src-tauri/src"] {
            let mut stack = vec![root.join(dir)];
            while let Some(path) = stack.pop() {
                let Ok(entries) = std::fs::read_dir(&path) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        stack.push(path);
                    } else if path.extension().is_some_and(|ext| ext == "rs") {
                        let text = std::fs::read_to_string(&path).unwrap();
                        let needle = concat!("Command::new(\"", "kill\")");
                        for (index, _) in text.match_indices(needle) {
                            let call: String = text[index..].lines().take(4).collect();
                            if call.contains("format!(\"-") && !call.contains("\"--\"") {
                                offenders.push(path.display().to_string());
                            }
                        }
                    }
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "negative pid without `--`: {offenders:?}"
        );
    }

    fn stop_test_service(pid: u32) {
        #[cfg(windows)]
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .status();
        // Signal the group directly: procps `kill -9 -123456` (no `--`) parses as
        // `kill(-1, SIGKILL)` and kills every process of the user.
        #[cfg(unix)]
        if let Some(group) = rustix::process::Pid::from_raw(pid as i32) {
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        }
    }
}
