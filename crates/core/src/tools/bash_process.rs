//! Bounded shell output and descendant lifetime management.
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{process::Stdio, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};
const OUTPUT_LIMIT: usize = 1024 * 1024;

async fn capture(
    mut pipe: impl AsyncRead + Unpin,
    output: &mut Vec<u8>,
    truncated: &mut bool,
    log: Option<std::path::PathBuf>,
) -> std::io::Result<()> {
    let mut log = log.map(std::fs::File::create).transpose()?;
    let mut buffer = [0u8; 8192];
    loop {
        let count = pipe.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        let keep = count.min(OUTPUT_LIMIT.saturating_sub(output.len()));
        output.extend_from_slice(&buffer[..keep]);
        if let Some(log) = log.as_mut() {
            std::io::Write::write_all(log, &buffer[..keep])?;
        }
        *truncated |= keep < count;
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
    let (mut stdout_bytes, mut stderr_bytes) = (Vec::new(), Vec::new());
    let (mut out_cut, mut err_cut) = (false, false);
    let (out_log, err_log) = logs
        .map(|(out, err)| (Some(out), Some(err)))
        .unwrap_or_default();
    let execution = async {
        let (status, out, err) = tokio::join!(
            child.wait(),
            capture(stdout, &mut stdout_bytes, &mut out_cut, out_log),
            capture(stderr, &mut stderr_bytes, &mut err_cut, err_log)
        );
        out?;
        err?;
        status
    };
    let result = match timeout {
        Some(timeout) => tokio::time::timeout(timeout, execution).await,
        None => Ok(execution.await),
    };
    // Closing the process tree also terminates descendants holding inherited pipes.
    drop(tree);
    match result {
        Ok(Ok(status)) => Ok(json!({
            "stdout":super::limit_output(&String::from_utf8_lossy(&stdout_bytes)),
            "stderr":super::limit_output(&String::from_utf8_lossy(&stderr_bytes)),
            "stdout_truncated":out_cut,"stderr_truncated":err_cut,
            "exit_code":status.code().unwrap_or(-1),"success":status.success(),"shell":shell
        })),
        Ok(Err(error)) => Err(error).context("Reading shell command output"),
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            Ok(
                json!({"error":format!("Command timed out after {} ms; descendants were terminated. For long installations or builds, set timeout explicitly (milliseconds, at most 600000), or use run_in_background.",timeout.unwrap().as_millis()),"is_error":true,"timed_out":true,"shell":shell,"stdout":super::limit_output(&String::from_utf8_lossy(&stdout_bytes)),"stderr":super::limit_output(&String::from_utf8_lossy(&stderr_bytes)),"stdout_truncated":out_cut,"stderr_truncated":err_cut}),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn output_is_bounded_while_both_pipes_are_drained() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let mut command = Command::new("node");
        command.args(["-e", "process.stdout.write('x'.repeat(2*1024*1024));process.stderr.write('y'.repeat(2*1024*1024))"]);
        let result = run(&mut command, Duration::from_secs(10), "node-fixture")
            .await
            .unwrap();
        assert_eq!(result["success"], true);
        assert_eq!(result["stdout_truncated"], true);
        assert_eq!(result["stderr_truncated"], true);
        assert!(result["stdout"].as_str().unwrap().len() <= OUTPUT_LIMIT + 200);
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
