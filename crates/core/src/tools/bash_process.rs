//! Bounded shell output and descendant lifetime management.
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};
const OUTPUT_LIMIT: usize = 1024 * 1024;

#[derive(Default)]
struct Captured {
    head: Vec<u8>,
    tail: std::collections::VecDeque<u8>,
    total: usize,
}
impl Captured {
    fn push(&mut self, bytes: &[u8]) {
        self.total = self.total.saturating_add(bytes.len());
        let head = bytes
            .len()
            .min((OUTPUT_LIMIT / 2).saturating_sub(self.head.len()));
        self.head.extend_from_slice(&bytes[..head]);
        let rest = &bytes[head..];
        let remove = (self.tail.len() + rest.len()).saturating_sub(OUTPUT_LIMIT / 2);
        self.tail.drain(..remove.min(self.tail.len()));
        self.tail.extend(rest.iter().copied());
    }

    fn text(&self) -> String {
        let mut bytes = self.head.clone();
        let tail: Vec<_> = self.tail.iter().copied().collect();
        let mut tail_start = 0;
        if self.total > OUTPUT_LIMIT {
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

    fn truncated(&self) -> bool {
        self.total > OUTPUT_LIMIT || self.text().len() > super::max_output_bytes()
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
    run_logged(command, Some(timeout), shell, None).await
}

pub(super) async fn run_logged(
    command: &mut Command,
    timeout: Option<Duration>,
    shell: &str,
    logs: Option<(std::path::PathBuf, std::path::PathBuf)>,
) -> Result<Value> {
    command
        .kill_on_drop(true)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command.spawn().context("Starting shell command")?;
    let tree = crate::services::tool_hooks::ProcessTree::new(
        child.id().context("Shell process ID missing")?,
    )?;
    let stdout = child.stdout.take().context("Shell stdout missing")?;
    let stderr = child.stderr.take().context("Shell stderr missing")?;
    let (mut stdout_bytes, mut stderr_bytes) = (Captured::default(), Captured::default());
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
        Ok(Ok(status)) => Ok(json!({
            "stdout":super::limit_output(&stdout_bytes.text()),
            "stderr":super::limit_output(&stderr_bytes.text()),
            "stdout_truncated":stdout_bytes.truncated(),"stderr_truncated":stderr_bytes.truncated(),
            "exit_code":status.code().unwrap_or(-1),"success":status.success(),"shell":shell
        })),
        Ok(Err(error)) => Err(error).context("Reading shell command output"),
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Ok(
                json!({"error":format!("Command timed out after {} ms; descendants were terminated. For long installations or builds, set timeout explicitly (milliseconds, at most 600000), or use run_in_background.",timeout.unwrap().as_millis()),"is_error":true,"timed_out":true,"shell":shell,"stdout":super::limit_output(&stdout_bytes.text()),"stderr":super::limit_output(&stderr_bytes.text()),"stdout_truncated":stdout_bytes.truncated(),"stderr_truncated":stderr_bytes.truncated()}),
            )
        }
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
}
