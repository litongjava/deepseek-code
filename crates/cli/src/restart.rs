//! `/restart`: replace this process with a fresh dcl on the same conversation.

use anyhow::{Context, Result};
use std::path::Path;

/// Arguments for the replacement process: this command line without session
/// selection, working directory or an initial prompt, plus the conversation to
/// resume and the folder it works in.
pub fn args(
    original: &[String],
    prompt: Option<&str>,
    session: &str,
    workspace: &Path,
) -> Vec<String> {
    let mut args = Vec::new();
    let mut prompt = prompt;
    let mut rest = original.iter().peekable();
    while let Some(arg) = rest.next() {
        let flag = arg.split('=').next().unwrap_or(arg);
        let inline = arg.contains('=');
        match flag {
            "-r" | "--resume" | "--session-id" | "--name" | "-C" | "--path" => {
                if !inline {
                    rest.next();
                }
            }
            "-w" | "--worktree" => {
                // `-C` already names the worktree the session runs in, and `-w`
                // conflicts with `--resume` in the replacement process.
                if !inline
                    && rest
                        .peek()
                        .is_some_and(|next| !next.starts_with('-') && Some(next.as_str()) != prompt)
                {
                    rest.next();
                }
            }
            "-c" | "--continue" | "--fork-session" | "--auto-route" => {}
            _ if prompt == Some(arg.as_str()) => prompt = None,
            _ => args.push(arg.clone()),
        }
    }
    args.extend([
        "-C".into(),
        workspace.to_string_lossy().into_owned(),
        "--resume".into(),
        session.into(),
    ]);
    args
}

fn executable() -> Result<std::path::PathBuf> {
    std::env::current_exe().context("Cannot find the dcl executable to restart")
}

/// Replaces this process so stdin, the console and signals stay with exactly
/// one dcl. Returns only when the replacement could not start.
#[cfg(unix)]
pub fn relaunch(args: &[String]) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let error = std::process::Command::new(executable()?).args(args).exec();
    Err(error).context("Cannot restart dcl")
}

/// Windows has no exec: stay as a thin wrapper so the shell waits for the
/// restarted dcl rather than returning a prompt beside it.
#[cfg(not(unix))]
pub fn relaunch(args: &[String]) -> Result<()> {
    let status = std::process::Command::new(executable()?)
        .args(args)
        .status()
        .context("Cannot restart dcl")?;
    std::process::exit(status.code().unwrap_or(1));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn arguments_resume_the_conversation_and_drop_session_selection() {
        let original = strings(&[
            "--model",
            "deepseek-flash",
            "-r",
            "old",
            "--name",
            "old title",
            "-C",
            "/old/dir",
            "--continue",
            "fix the bug",
        ]);
        assert_eq!(
            args(
                &original,
                Some("fix the bug"),
                "new",
                Path::new("/work/tree")
            ),
            strings(&[
                "--model",
                "deepseek-flash",
                "-C",
                "/work/tree",
                "--resume",
                "new"
            ])
        );
    }

    #[test]
    fn inline_forms_and_worktrees_are_dropped() {
        let original = strings(&[
            "--resume=old",
            "--name=old",
            "-w",
            "feature",
            "--verbose",
            "task",
        ]);
        assert_eq!(
            args(&original, Some("task"), "new", Path::new("/work/tree")),
            strings(&["--verbose", "-C", "/work/tree", "--resume", "new"])
        );
        // A bare `-w` would conflict with `--resume`; the workspace covers it.
        let original = strings(&["-w", "--verbose"]);
        assert_eq!(
            args(&original, None, "new", Path::new("/work/tree")),
            strings(&["--verbose", "-C", "/work/tree", "--resume", "new"])
        );
    }

    #[test]
    fn launch_wide_options_survive_the_restart() {
        let original = strings(&[
            "--permission-mode",
            "accept-edits",
            "--allowed-tools",
            "Bash",
            "Read",
        ]);
        assert_eq!(
            args(&original, None, "new", Path::new("/work/tree")),
            strings(&[
                "--permission-mode",
                "accept-edits",
                "--allowed-tools",
                "Bash",
                "Read",
                "-C",
                "/work/tree",
                "--resume",
                "new",
            ])
        );
    }
}
