//! The Git status snapshot the reference CLI attaches when a conversation
//! starts. It is taken once per session and reused for every later request,
//! so the prompt prefix stays stable for caching.
use std::path::Path;

const MAX_STATUS_CHARS: usize = 2000;

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git").args(args).current_dir(cwd).stdin(std::process::Stdio::null()).output().ok()?;
    output.status.success().then(|| String::from_utf8_lossy(&output.stdout).trim_end().to_owned())
}

fn main_branch(cwd: &Path) -> String {
    if let Some(head) = git(cwd, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"]) {
        return head.trim_start_matches("origin/").to_owned();
    }
    for candidate in ["main", "master"] {
        if git(cwd, &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{candidate}")]).is_some() {
            return candidate.to_owned();
        }
    }
    "main".to_owned()
}

/// The snapshot text, or None outside a Git work tree.
pub fn take(cwd: &Path) -> Option<String> {
    if git(cwd, &["rev-parse", "--is-inside-work-tree"])?.trim() != "true" { return None; }
    let branch = git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
    let user = git(cwd, &["config", "user.name"]).filter(|name| !name.is_empty());
    let mut status = git(cwd, &["status", "--short"]).unwrap_or_default();
    if status.chars().count() > MAX_STATUS_CHARS {
        status = status.chars().take(MAX_STATUS_CHARS).collect::<String>() + "\n... (truncated because it exceeds 2k characters. If you need more information, run \"git status\" using Bash)";
    }
    let commits = git(cwd, &["log", "--oneline", "-n", "5"]).unwrap_or_default();
    let mut text = format!("# gitStatus\nThis is the git status at the start of the conversation. Note that this status is a snapshot in time, and will not update during the conversation.\n\nCurrent branch: {branch}\n\nMain branch (you will usually use this for PRs): {}\n", main_branch(cwd));
    if let Some(user) = user { text.push_str(&format!("\nGit user: {user}\n")); }
    text.push_str(&format!("\nStatus:\n{}\n", if status.is_empty() { "(clean)" } else { &status }));
    if !commits.is_empty() { text.push_str(&format!("\nRecent commits:\n{commits}\n")); }
    Some(text)
}

/// The session's snapshot: taken on its first request, then reused.
pub fn for_session(root: &Path, session: &str, cwd: &Path) -> Option<String> {
    let path = root.join("git-status").join(format!("{session}.txt"));
    if let Ok(saved) = std::fs::read_to_string(&path) {
        return Some(saved).filter(|text| !text.is_empty());
    }
    let snapshot = take(cwd);
    if let Some(parent) = path.parent() { let _ = std::fs::create_dir_all(parent); }
    // An empty file records "not a repository" so it is not rechecked each turn.
    let _ = std::fs::write(&path, snapshot.as_deref().unwrap_or_default());
    snapshot
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_is_taken_once_per_session() {
        let root = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| assert!(std::process::Command::new("git").args(args).current_dir(repo.path()).output().unwrap().status.success());
        run(&["init", "-q", "-b", "main"]);
        run(&["-c", "user.name=Fixture", "-c", "user.email=f@example.invalid", "commit", "-q", "--allow-empty", "-m", "first commit"]);
        std::fs::write(repo.path().join("new.txt"), "x").unwrap();
        let first = for_session(root.path(), "s", repo.path()).unwrap();
        assert!(first.contains("Current branch: main") && first.contains("Main branch (you will usually use this for PRs): main"));
        assert!(first.contains("?? new.txt") && first.contains("first commit"));
        std::fs::remove_file(repo.path().join("new.txt")).unwrap();
        assert_eq!(for_session(root.path(), "s", repo.path()).unwrap(), first);
        let plain = tempfile::tempdir().unwrap();
        assert!(for_session(root.path(), "t", plain.path()).is_none());
        assert!(for_session(root.path(), "t", plain.path()).is_none());
    }
}
