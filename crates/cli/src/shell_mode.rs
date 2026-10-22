//! `!command` at the prompt: run a shell command yourself, as in the reference
//! CLI. The command and its output join the conversation so the model sees
//! them on the next turn; no approval is asked because the user typed it.
use crate::session::Session;
use anyhow::{Context, Result};

/// Characters of each stream kept in the conversation.
const MAX_STREAM: usize = 30_000;

fn shell() -> (std::path::PathBuf, Vec<&'static str>) {
    if cfg!(windows) {
        let program = claude_core::tools::lsp_servers::find_on_path("pwsh")
            .or_else(|| claude_core::tools::lsp_servers::find_on_path("powershell"))
            .unwrap_or_else(|| "powershell".into());
        (program, vec!["-NoProfile", "-Command"])
    } else {
        let program = claude_core::tools::lsp_servers::find_on_path("bash").unwrap_or_else(|| "sh".into());
        (program, vec!["-c"])
    }
}

fn clip(text: &str) -> String {
    let trimmed = text.trim_end();
    match trimmed.char_indices().nth(MAX_STREAM) {
        Some((end, _)) => format!("{}\n[output truncated]", &trimmed[..end]),
        None => trimmed.to_owned(),
    }
}

/// The transcript entry, in the reference CLI's format.
pub fn transcript(command: &str, stdout: &str, stderr: &str) -> String {
    format!(
        "<bash-input>{command}</bash-input>\n<bash-stdout>{}</bash-stdout><bash-stderr>{}</bash-stderr>",
        clip(stdout),
        clip(stderr)
    )
}

pub async fn run(session: &Session, command: &str) -> Result<()> {
    let command = command.trim();
    anyhow::ensure!(!command.is_empty(), "Type a command after !, e.g. !git status");
    let (program, args) = shell();
    let mut child = tokio::process::Command::new(&program);
    child
        .args(&args)
        .arg(command)
        .current_dir(&session.workspace)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let child = child.spawn().with_context(|| format!("Cannot start {}", program.display()))?;
    let output = tokio::select! {
        output = child.wait_with_output() => output?,
        _ = tokio::signal::ctrl_c() => anyhow::bail!("Command interrupted"),
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let color = std::env::var_os("NO_COLOR").is_none();
    let mut text = String::new();
    let mut first = true;
    for line in stdout.lines().chain(stderr.lines()) {
        let marker = if std::mem::take(&mut first) { "  \u{23bf}  " } else { "     " };
        text.push_str(&format!("{marker}{line}\n"));
    }
    if first {
        text.push_str("  \u{23bf}  (No output)\n");
    }
    if !output.status.success() {
        text.push_str(&format!("     Exit code {}\n", output.status.code().unwrap_or(-1)));
    }
    if color {
        eprint!("\x1b[38;2;253;93;177m!\x1b[39m {command}\n\x1b[90m{text}\x1b[39m");
    } else {
        eprint!("! {command}\n{text}");
    }
    session.store.append(&session.id, "user", &transcript(command, &stdout, &stderr))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_matches_the_reference_format_and_is_bounded() {
        assert_eq!(
            transcript("git status", "clean\n", ""),
            "<bash-input>git status</bash-input>\n<bash-stdout>clean</bash-stdout><bash-stderr></bash-stderr>"
        );
        let long = "x".repeat(MAX_STREAM + 10);
        assert!(transcript("cat big", &long, "").contains("[output truncated]"));
    }
}
