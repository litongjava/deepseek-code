//! Explicit skill invocation planning and permission-bound shell expansion.
//! Discovery and planning never start a process.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::{path::Path, process::Stdio, time::Duration};
use tokio::io::AsyncReadExt;

#[derive(Debug)]
pub enum Segment {
    Text(String),
    Shell(String),
}

pub fn parse(text: &str) -> Result<Vec<Segment>> {
    ensure!(text.len() <= 1024 * 1024, "Expanded skill exceeds 1 MiB");
    let pattern = regex::Regex::new(r"(?s)```!\s*(.*?)```|!`([^`]+)`")?;
    let mut segments = Vec::new();
    let mut offset = 0;
    let mut count = 0;
    for captures in pattern.captures_iter(text) {
        let full = captures.get(0).unwrap();
        let block = captures.get(1).is_some();
        if !block
            && full.start() > 0
            && !text[..full.start()].chars().last().unwrap().is_whitespace()
        {
            continue;
        }
        let command = captures
            .get(if block { 1 } else { 2 })
            .unwrap()
            .as_str()
            .trim();
        if command.is_empty() {
            continue;
        }
        count += 1;
        ensure!(count <= 16, "Skill contains more than 16 shell expansions");
        segments.push(Segment::Text(text[offset..full.start()].into()));
        segments.push(Segment::Shell(command.into()));
        offset = full.end();
    }
    segments.push(Segment::Text(text[offset..].into()));
    Ok(segments)
}

pub fn shell_input(command: &str, shell: Option<&str>) -> Result<Value> {
    let shell = shell.unwrap_or("bash");
    ensure!(
        ["bash", "sh", "powershell", "pwsh"].contains(&shell),
        "Unsupported skill shell: {shell}"
    );
    Ok(json!({"command":command,"shell":shell,"timeout":30_000}))
}

/// A permit binds the exact checked input to one process launch. Explicit deny
/// cannot be overridden by a confirmation, and permits cannot be reused.
pub struct ShellPermit {
    input: Value,
}
pub fn authorize(
    input: Value,
    decision: crate::permissions::PermissionResult,
    confirmed: bool,
) -> Result<ShellPermit> {
    match decision {
        crate::permissions::PermissionResult::Denied(reason) => {
            bail!("Skill shell denied: {reason}")
        }
        crate::permissions::PermissionResult::RequiresConfirmation(_) => {
            ensure!(confirmed, "Skill shell permission was not granted")
        }
        crate::permissions::PermissionResult::Granted => {}
    }
    let normalized = shell_input(
        input["command"].as_str().context("Skill command missing")?,
        input["shell"].as_str(),
    )?;
    ensure!(
        input == normalized,
        "Skill shell input changed after approval"
    );
    Ok(ShellPermit { input })
}

pub async fn execute(permit: ShellPermit, workspace: &Path) -> Result<String> {
    let script = permit.input["command"]
        .as_str()
        .context("Missing command")?;
    ensure!(
        !crate::tools::is_dangerous_command(script),
        "Skill command blocked by the native shell security filter"
    );
    let shell = permit.input["shell"].as_str().context("Missing shell")?;
    let mut command = tokio::process::Command::new(if shell == "powershell" && cfg!(windows) {
        "powershell.exe"
    } else {
        shell
    });
    if matches!(shell, "powershell" | "pwsh") {
        command.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"]);
        command.arg(format!("[Console]::InputEncoding = [Text.UTF8Encoding]::new($false); [Console]::OutputEncoding = [Text.UTF8Encoding]::new($false); $OutputEncoding = [Console]::OutputEncoding; {script}"));
    } else {
        command.args(["-c", script]);
    }
    command
        .current_dir(workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .with_context(|| format!("Starting skill shell {shell}"))?;
    let _tree =
        crate::services::tool_hooks::ProcessTree::new(child.id().context("Skill shell exited")?)?;
    let stdout = child.stdout.take().context("Missing stdout")?;
    let stderr = child.stderr.take().context("Missing stderr")?;
    async fn read(reader: impl tokio::io::AsyncRead + Unpin) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        reader.take(65537).read_to_end(&mut bytes).await?;
        ensure!(bytes.len() <= 65536, "Skill shell output exceeds 64 KiB");
        Ok(bytes)
    }
    tokio::time::timeout(Duration::from_secs(30), async {
        let (stdout, stderr) = tokio::try_join!(read(stdout), read(stderr))?;
        let status = child.wait().await?;
        ensure!(
            status.success(),
            "Skill shell failed with exit code {:?}: {}",
            status.code(),
            String::from_utf8_lossy(&stderr)
        );
        let mut output = String::from_utf8(stdout).context("Skill shell stdout is not UTF-8")?;
        if !stderr.is_empty() {
            output.push_str("\n[stderr]\n");
            output.push_str(&String::from_utf8(stderr).context("Skill shell stderr is not UTF-8")?);
        }
        Ok(output)
    })
    .await
    .context("Skill shell timed out")?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::PermissionResult;
    #[test]
    fn parsing_is_pure_and_preserves_adjacent_inline_markdown() {
        let parts =
            parse("prefix !`echo one`\n```!\necho two\n```\n`foo`!`bar` $!`variable`").unwrap();
        let commands: Vec<_> = parts
            .iter()
            .filter_map(|part| {
                if let Segment::Shell(command) = part {
                    Some(command.as_str())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(commands, ["echo one", "echo two"]);
        assert!(matches!(parts.last(),Some(Segment::Text(text)) if text.contains("`foo`!`bar`")));
    }
    #[tokio::test]
    async fn approved_shell_returns_real_unicode_and_denial_cannot_launch() {
        let temp = tempfile::tempdir().unwrap();
        let (shell, command) = if cfg!(windows) {
            ("powershell", "[Console]::Write('中文😀 $literal')")
        } else {
            ("sh", "printf '中文😀 $literal'")
        };
        let input = shell_input(command, Some(shell)).unwrap();
        assert!(authorize(
            input.clone(),
            PermissionResult::Denied("policy".into()),
            true
        )
        .is_err());
        assert!(authorize(
            input.clone(),
            PermissionResult::RequiresConfirmation("ask".into()),
            false
        )
        .is_err());
        let permit = authorize(
            input,
            PermissionResult::RequiresConfirmation("ask".into()),
            true,
        )
        .unwrap();
        assert_eq!(
            execute(permit, temp.path()).await.unwrap(),
            "中文😀 $literal"
        );
    }
    #[tokio::test]
    async fn oversized_shell_output_is_rejected() {
        let temp = tempfile::tempdir().unwrap();
        let (shell, command) = if cfg!(windows) {
            ("powershell", "[Console]::Write(('x' * 70000))")
        } else {
            ("sh", "head -c 70000 /dev/zero")
        };
        let permit = authorize(
            shell_input(command, Some(shell)).unwrap(),
            PermissionResult::Granted,
            false,
        )
        .unwrap();
        assert!(execute(permit, temp.path())
            .await
            .unwrap_err()
            .to_string()
            .contains("64 KiB"));
    }

    #[tokio::test]
    async fn cancelling_expansion_stops_the_process_before_later_side_effects() {
        let temp = tempfile::tempdir().unwrap();
        let (shell, script) = if cfg!(windows) {
            ("powershell", "[IO.File]::WriteAllText((Join-Path (Get-Location) 'ready'), 'yes'); Start-Sleep -Milliseconds 600; [IO.File]::WriteAllText((Join-Path (Get-Location) 'late'), 'bad')")
        } else {
            ("sh", "printf yes > ready; sleep 0.6; printf bad > late")
        };
        let permit = authorize(
            shell_input(script, Some(shell)).unwrap(),
            PermissionResult::Granted,
            false,
        )
        .unwrap();
        let workspace = temp.path().to_path_buf();
        let task = tokio::spawn(async move { execute(permit, &workspace).await });
        let ready = tokio::time::timeout(Duration::from_secs(5), async {
            while !temp.path().join("ready").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        task.abort();
        let _ = task.await;
        ready.expect("Fixture shell did not reach ready state");
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert!(!temp.path().join("late").exists());
    }
}
