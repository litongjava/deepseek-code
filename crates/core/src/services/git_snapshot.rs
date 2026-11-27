//! The Git status snapshot the reference CLI attaches when a conversation
//! starts. It is taken once per session and reused for every later request,
//! so the prompt prefix stays stable for caching. A workspace that is not a
//! repository reports its immediate subdirectory repositories instead.
use std::path::Path;

const MAX_STATUS_CHARS: usize = 2000;
/// One repository below a non-repository workspace gets less room so several
/// snapshots stay inside the first-message budget.
const MAX_SUBDIR_STATUS_CHARS: usize = 600;
const MAX_SUBDIR_COMMITS: usize = 3;
/// Directories examined for nested repositories, and repositories reported.
const MAX_SUBDIRECTORIES: usize = 64;
const MAX_SUBDIR_REPOS: usize = 8;
const MAX_SNAPSHOT_CHARS: usize = 8000;

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

fn work_tree(cwd: &Path) -> bool {
    git(cwd, &["rev-parse", "--is-inside-work-tree"]).is_some_and(|text| text.trim() == "true")
}

fn truncated_status(cwd: &Path, limit: usize) -> String {
    let mut status = git(cwd, &["status", "--short"]).unwrap_or_default();
    if status.chars().count() > limit {
        status = status.chars().take(limit).collect::<String>();
        status.push_str(&format!("\n... (truncated because it exceeds {limit} characters. If you need more information, run \"git status\" using Bash)"));
    }
    status
}

/// One repository's details without the snapshot heading.
fn repository(cwd: &Path, status_limit: usize, commit_count: usize, user: bool) -> Option<String> {
    if !work_tree(cwd) { return None; }
    let branch = git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap_or_default();
    let mut text = format!("Current branch: {branch}\n\nMain branch (you will usually use this for PRs): {}\n", main_branch(cwd));
    if user {
        if let Some(name) = git(cwd, &["config", "user.name"]).filter(|name| !name.is_empty()) {
            text.push_str(&format!("\nGit user: {name}\n"));
        }
    }
    let status = truncated_status(cwd, status_limit);
    text.push_str(&format!("\nStatus:\n{}\n", if status.is_empty() { "(clean)" } else { &status }));
    let commits = git(cwd, &["log", "--oneline", "-n", &commit_count.to_string()]).unwrap_or_default();
    if !commits.is_empty() { text.push_str(&format!("\nRecent commits:\n{commits}\n")); }
    Some(text)
}

/// Immediate subdirectories that look like Git work trees (a `.git` directory
/// or worktree file), sorted by name. Only one level is inspected.
fn child_repositories(cwd: &Path) -> Vec<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(cwd) else { return Vec::new() };
    let mut directories: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| !name.starts_with('.'))
        })
        .filter(|path| {
            std::fs::symlink_metadata(path)
                .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
        })
        .filter(|path| path.join(".git").exists())
        .collect();
    directories.sort();
    directories.truncate(MAX_SUBDIRECTORIES);
    directories
}

/// The snapshot text, or None outside a Git work tree and without immediate
/// subdirectory repositories.
pub fn take(cwd: &Path) -> Option<String> {
    let heading = "# gitStatus\nThis is the git status at the start of the conversation. Note that this status is a snapshot in time, and will not update during the conversation.\n";
    if work_tree(cwd) {
        return Some(format!("{heading}\n{}", repository(cwd, MAX_STATUS_CHARS, 5, true)?));
    }
    let mut sections = Vec::new();
    for repository_path in child_repositories(cwd) {
        if sections.len() >= MAX_SUBDIR_REPOS { break; }
        let Some(name) = repository_path.file_name().and_then(|name| name.to_str()) else { continue };
        if let Some(details) = repository(&repository_path, MAX_SUBDIR_STATUS_CHARS, MAX_SUBDIR_COMMITS, false) {
            sections.push(format!("## {name}\n{details}"));
        }
    }
    if sections.is_empty() { return None; }
    let mut text = format!(
        "{heading}\nThe workspace is not itself a Git repository; these snapshots cover its immediate subdirectories (up to {MAX_SUBDIR_REPOS} repositories).\n\n{}",
        sections.join("\n")
    );
    if text.chars().count() > MAX_SNAPSHOT_CHARS {
        text = text.chars().take(MAX_SNAPSHOT_CHARS).collect::<String>();
        text.push_str("\n... (truncated because the snapshot exceeds the size limit. If you need more information, run \"git status\" using Bash)");
    }
    Some(text)
}

/// The snapshot as the reference CLI attaches it to the first user message.
pub fn reminder(snapshot: &str) -> String {
    format!("<system-reminder>\nAs you answer the user's questions, you can use the following context:\n{}\n\ndeepseek-code attached this context automatically; it isn't part of the user's message. It describes the user's own account and workspace, so they don't need it reported back.\n</system-reminder>", snapshot.trim_end())
}

/// The session's snapshot: taken on its first request, then reused.
pub fn for_session(root: &Path, session: &str, cwd: &Path) -> Option<String> {
    let path = root.join("cache/git-status").join(format!("{session}.txt"));
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
    fn subdirectory_repositories_are_snapshotted_when_the_workspace_is_not_one() {
        let workspace = tempfile::tempdir().unwrap();
        for name in ["deploy", "deploy-docs"] {
            let repo = workspace.path().join(name);
            std::fs::create_dir(&repo).unwrap();
            let run = |args: &[&str]| {
                assert!(std::process::Command::new("git").args(args).current_dir(&repo).output().unwrap().status.success());
            };
            run(&["init", "-q", "-b", "main"]);
            let message = format!("{name} first");
            run(&["-c", "user.name=Fixture", "-c", "user.email=f@example.invalid", "commit", "-q", "--allow-empty", "-m", &message]);
            std::fs::write(repo.join("new.txt"), "x").unwrap();
        }
        std::fs::create_dir(workspace.path().join("plain")).unwrap();

        let text = take(workspace.path()).unwrap();
        assert!(text.contains("The workspace is not itself a Git repository"), "{text}");
        assert!(text.contains("## deploy\n") && text.contains("## deploy-docs\n"), "{text}");
        assert!(text.contains("deploy first") && text.contains("deploy-docs first"), "{text}");
        assert_eq!(text.matches("Current branch: main").count(), 2);
        assert!(text.contains("?? new.txt"));
        assert!(!text.contains("## plain"));

        // A workspace with no repository and no subdirectory repository stays empty.
        let empty = tempfile::tempdir().unwrap();
        assert!(take(empty.path()).is_none());
    }

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
