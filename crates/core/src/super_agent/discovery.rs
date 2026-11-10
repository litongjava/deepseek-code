//! Bounded Git discovery for Super Agent task targets.
//!
//! The result distinguishes a usable repository, an explicitly non-Git target,
//! a repository that is not ready for isolation, and a detection failure. A
//! failed command is never reinterpreted as "no Git": guessing there would make
//! the orchestrator modify a directory without the isolation the user expects.
use crate::worktree::session::command;
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    path::{Component, Path, PathBuf},
    process::Output,
};

#[derive(Debug, Clone)]
pub struct DiscoveryOptions {
    /// A target must resolve inside one of these roots. An empty list denies
    /// every target rather than silently treating the whole filesystem as allowed.
    pub authorized_roots: Vec<PathBuf>,
    pub max_depth: usize,
    pub max_entries: usize,
    pub skip_directories: Vec<String>,
}

impl Default for DiscoveryOptions {
    fn default() -> Self {
        Self {
            authorized_roots: Vec::new(),
            max_depth: 6,
            max_entries: 2_000,
            skip_directories: [
                ".git",
                ".claude",
                "node_modules",
                "target",
                "dist",
                "build",
                ".venv",
                "venv",
                "vendor",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitRepository {
    /// The requested path, which may be a subdirectory or a not-yet-created path.
    pub target_path: PathBuf,
    pub root: PathBuf,
    pub common_dir: PathBuf,
    pub git_dir: PathBuf,
    pub current_branch: Option<String>,
    pub detached: bool,
    /// Set for a submodule checkout.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub superproject: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Discovery {
    GitRepository(GitRepository),
    NonGit {
        target: PathBuf,
    },
    GitSetupRequired {
        target: PathBuf,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        root: Option<PathBuf>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        common_dir: Option<PathBuf>,
        reason: String,
    },
    DetectionFailed {
        target: PathBuf,
        error: String,
    },
}

impl Discovery {
    pub fn target(&self) -> &Path {
        match self {
            Self::GitRepository(repo) => &repo.target_path,
            Self::NonGit { target }
            | Self::GitSetupRequired { target, .. }
            | Self::DetectionFailed { target, .. } => target,
        }
    }

    pub fn repository(&self) -> Option<&GitRepository> {
        match self {
            Self::GitRepository(repo) => Some(repo),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanOutcome {
    pub discoveries: Vec<Discovery>,
    pub truncated: bool,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceState {
    pub branch: Option<String>,
    pub head: Option<String>,
    pub dirty: bool,
    pub entries: Vec<String>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BranchSelectionError {
    #[error("configured target branch does not exist: {0}")]
    Missing(String),
    #[error("invalid target branch name: {0}")]
    Invalid(String),
    #[error("the repository is on a detached HEAD; select a target branch")]
    Detached,
    #[error("Git could not resolve the target branch: {0}")]
    Git(String),
}

/// Detects one target. Detection errors are returned as `DetectionFailed`,
/// never as `NonGit`.
pub fn discover_target(target: &Path, options: &DiscoveryOptions) -> Discovery {
    match detect_target(target, options) {
        Ok(discovery) => discovery,
        Err(error) => Discovery::DetectionFailed {
            target: target.to_path_buf(),
            error: format!("{error:#}"),
        },
    }
}

fn detect_target(target: &Path, options: &DiscoveryOptions) -> Result<Discovery> {
    let target = absolute(target)?;
    let ancestor = nearest_existing_ancestor(&target)?;
    let canonical_ancestor = ancestor
        .canonicalize()
        .with_context(|| format!("Resolving target ancestor {}", ancestor.display()))?;
    authorize(&canonical_ancestor, options)?;
    let probe = command(&canonical_ancestor, &["rev-parse", "--is-inside-work-tree"])
        .context("Running Git discovery")?;
    if !probe.status.success() {
        let stderr = String::from_utf8_lossy(&probe.stderr).to_lowercase();
        if stderr.contains("not a git repository") {
            return Ok(Discovery::NonGit { target });
        }
        return Ok(Discovery::DetectionFailed {
            target,
            error: format!(
                "Git discovery failed: {}",
                String::from_utf8_lossy(&probe.stderr).trim()
            ),
        });
    }
    if String::from_utf8_lossy(&probe.stdout).trim() != "true" {
        return Ok(Discovery::GitSetupRequired {
            target,
            root: None,
            common_dir: None,
            reason: "target is not inside a Git working tree".into(),
        });
    }
    let root = PathBuf::from(git_ok(
        &canonical_ancestor,
        &["rev-parse", "--show-toplevel"],
    )?);
    let common_dir = absolute_git_dir(&canonical_ancestor, "--git-common-dir")?;
    let git_dir = absolute_git_dir(&canonical_ancestor, "--absolute-git-dir")
        .unwrap_or_else(|_| common_dir.clone());
    let root = root.canonicalize().unwrap_or(root);
    if let Some(reason) = repository_setup_requirement(&canonical_ancestor, &git_dir)? {
        return Ok(Discovery::GitSetupRequired {
            target,
            root: Some(root),
            common_dir: Some(common_dir),
            reason,
        });
    }
    let current_branch = command(
        &canonical_ancestor,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
    )
    .ok()
    .filter(|output| output.status.success())
    .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    .filter(|branch| !branch.is_empty());
    let detached = current_branch.is_none();
    let superproject = command(
        &canonical_ancestor,
        &["rev-parse", "--show-superproject-working-tree"],
    )
    .ok()
    .filter(|output| output.status.success())
    .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    .filter(|path| !path.is_empty())
    .map(PathBuf::from)
    .and_then(|path| path.canonicalize().ok());
    Ok(Discovery::GitRepository(GitRepository {
        target_path: target,
        root,
        common_dir,
        git_dir,
        current_branch,
        detached,
        superproject,
    }))
}

fn absolute_git_dir(cwd: &Path, argument: &str) -> Result<PathBuf> {
    let path = PathBuf::from(git_ok(
        cwd,
        &["rev-parse", "--path-format=absolute", argument],
    )?);
    let path = if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    };
    Ok(path.canonicalize().unwrap_or(path))
}

fn repository_setup_requirement(cwd: &Path, git_dir: &Path) -> Result<Option<String>> {
    let head = command(cwd, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    if !head.status.success() {
        let symbolic = command(cwd, &["symbolic-ref", "--quiet", "HEAD"])?;
        let stderr = String::from_utf8_lossy(&head.stderr).to_lowercase();
        if symbolic.status.success()
            || stderr.contains("needed a single revision")
            || stderr.contains("unknown revision")
        {
            return Ok(Some("repository has no commits yet".into()));
        }
        return Ok(Some(format!(
            "Git cannot resolve HEAD: {}",
            String::from_utf8_lossy(&head.stderr).trim()
        )));
    }
    for (path, state) in [
        ("MERGE_HEAD", "an unfinished merge"),
        ("CHERRY_PICK_HEAD", "an unfinished cherry-pick"),
        ("REVERT_HEAD", "an unfinished revert"),
        ("BISECT_LOG", "an unfinished bisect"),
        ("rebase-merge", "an unfinished rebase"),
        ("rebase-apply", "an unfinished rebase"),
    ] {
        if git_dir.join(path).exists() {
            return Ok(Some(format!("repository is in {state}")));
        }
    }
    Ok(None)
}

fn absolute(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn nearest_existing_ancestor(path: &Path) -> Result<PathBuf> {
    let mut current = path.to_path_buf();
    loop {
        if current.exists() {
            return Ok(current);
        }
        let parent = current
            .parent()
            .context("Target has no existing ancestor")?
            .to_path_buf();
        if parent == current {
            anyhow::bail!("Target has no existing ancestor");
        }
        current = parent;
    }
}

fn authorize(path: &Path, options: &DiscoveryOptions) -> Result<()> {
    ensure!(
        !options.authorized_roots.is_empty(),
        "No authorized roots are configured for repository discovery"
    );
    let mut usable = 0;
    for root in &options.authorized_roots {
        let Ok(root) = root.canonicalize() else {
            continue;
        };
        usable += 1;
        if path.starts_with(&root) {
            return Ok(());
        }
    }
    ensure!(usable > 0, "No configured authorized root is available");
    anyhow::bail!("Target {} is outside the authorized roots", path.display())
}

fn git_ok(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = command(cwd, args)?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

/// Scans a workspace without following symlinks. It stops after `max_entries`
/// visited entries and skips dependency/build directories by name.
pub fn scan_repositories(root: &Path, options: &DiscoveryOptions) -> Result<ScanOutcome> {
    let root = root
        .canonicalize()
        .with_context(|| format!("Resolving scan root {}", root.display()))?;
    ensure!(root.is_dir(), "Scan root must be a directory");
    authorize(&root, options)?;
    let mut discoveries = Vec::new();
    let mut errors = Vec::new();
    let mut truncated = false;
    let mut seen_common = std::collections::BTreeSet::new();
    let mut visited = 0usize;

    if let Discovery::GitRepository(repo) = discover_target(&root, options) {
        record_repository(&mut discoveries, &mut seen_common, repo);
    }

    let mut stack = vec![(root.clone(), 0usize)];
    'walk: while let Some((directory, depth)) = stack.pop() {
        let mut entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries
                .filter_map(|entry| match entry {
                    Ok(entry) => Some(entry),
                    Err(error) => {
                        errors.push(format!("{}: {error}", directory.display()));
                        None
                    }
                })
                .collect::<Vec<_>>(),
            Err(error) => {
                errors.push(format!("{}: {error}", directory.display()));
                continue;
            }
        };
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            visited += 1;
            if visited > options.max_entries {
                truncated = true;
                break 'walk;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if options.skip_directories.contains(&name) {
                continue;
            }
            let file_type = match entry.file_type() {
                Ok(file_type) => file_type,
                Err(error) => {
                    errors.push(format!("{}: {error}", entry.path().display()));
                    continue;
                }
            };
            // Never follow links: they can create cycles or escape the root.
            if file_type.is_symlink() || !file_type.is_dir() {
                continue;
            }
            let path = entry.path();
            if path.join(".git").symlink_metadata().is_ok() {
                match discover_target(&path, options) {
                    Discovery::GitRepository(repo) => {
                        record_repository(&mut discoveries, &mut seen_common, repo);
                    }
                    other => discoveries.push(other),
                }
            }
            if depth + 1 <= options.max_depth {
                stack.push((path, depth + 1));
            }
        }
    }
    Ok(ScanOutcome {
        discoveries,
        truncated,
        errors,
    })
}

fn record_repository(
    discoveries: &mut Vec<Discovery>,
    seen_common: &mut std::collections::BTreeSet<String>,
    repo: GitRepository,
) -> bool {
    let key = repo.common_dir.to_string_lossy().into_owned();
    if !seen_common.insert(key) {
        return false;
    }
    discoveries.push(Discovery::GitRepository(repo));
    true
}

/// Reads the source worktree state so later integration can refuse to merge
/// over user changes. It never modifies the source.
pub fn source_state(repo_root: &Path) -> Result<SourceState> {
    let output = command(
        repo_root,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )?;
    ensure!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr).trim()
    );
    let mut entries: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .split('\0')
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .collect();
    entries.sort();
    let branch = command(repo_root, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|value| !value.is_empty());
    let head = command(repo_root, &["rev-parse", "--verify", "HEAD^{commit}"])
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned());
    Ok(SourceState {
        branch,
        head,
        dirty: !entries.is_empty(),
        entries,
    })
}

/// Selects the branch a task will integrate into. A configured value wins and
/// must exist; otherwise the repository's current branch is recorded. `main` is
/// never assumed, and a detached HEAD asks the caller to choose.
pub fn select_target_branch(
    repo: &GitRepository,
    configured: Option<&str>,
) -> Result<String, BranchSelectionError> {
    if let Some(branch) = configured {
        validate_branch_name(branch)?;
        let output = command(
            &repo.root,
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/heads/{branch}"),
            ],
        )
        .map_err(|error| BranchSelectionError::Git(error.to_string()))?;
        if !output.status.success() {
            return Err(BranchSelectionError::Missing(branch.into()));
        }
        return Ok(branch.into());
    }
    repo.current_branch
        .clone()
        .ok_or(BranchSelectionError::Detached)
}

pub fn validate_branch_name(name: &str) -> Result<(), BranchSelectionError> {
    let invalid = name.is_empty()
        || name.len() > 200
        || name.starts_with('-')
        || name.starts_with('/')
        || name.ends_with('/')
        || name.ends_with('.')
        || name.contains("..")
        || name.contains("//")
        || name.contains("@{")
        || name.ends_with(".lock")
        || name
            .chars()
            .any(|ch| ch.is_whitespace() || "~^:?*[\\".contains(ch));
    if invalid {
        return Err(BranchSelectionError::Invalid(name.into()));
    }
    Ok(())
}

/// A lexical safety check for a path supplied by a model or stored record.
pub fn is_plain_relative_path(path: &str) -> bool {
    let path = Path::new(path);
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(cwd: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .env("LC_ALL", "C")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn repo(name: &str) -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(name);
        std::fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.email", "test@example.invalid"]);
        git(&root, &["config", "user.name", "Test"]);
        std::fs::write(root.join("file.txt"), "main\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", "initial"]);
        (temp, root.canonicalize().unwrap())
    }

    fn options(root: &Path) -> DiscoveryOptions {
        DiscoveryOptions {
            authorized_roots: vec![root.to_path_buf()],
            ..DiscoveryOptions::default()
        }
    }

    #[test]
    fn non_git_parent_and_plain_directory_are_reported_without_creating_git() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let scripts = root.join("scripts");
        std::fs::create_dir(&scripts).unwrap();
        std::fs::write(scripts.join("run.sh"), "echo hi\n").unwrap();
        let discovery = discover_target(&scripts, &options(&root));
        assert_eq!(
            discovery,
            Discovery::NonGit {
                target: scripts.clone()
            }
        );
        assert!(!scripts.join(".git").exists());
        assert!(!root.join(".git").exists());
    }

    #[test]
    fn detection_errors_are_not_reported_as_non_git() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let file = root.join("not-a-directory");
        std::fs::write(&file, "x").unwrap();
        let target = file.join("child");
        match discover_target(&target, &options(&root)) {
            Discovery::DetectionFailed { error, .. } => assert!(!error.is_empty()),
            other => panic!("expected detection failure, got {other:?}"),
        }
        // A target outside the authorized roots is a setup error, not NonGit.
        let outside = tempfile::tempdir().unwrap();
        match discover_target(outside.path(), &options(&root)) {
            Discovery::DetectionFailed { error, .. } => {
                assert!(error.contains("outside the authorized roots"))
            }
            other => panic!("expected authorization failure, got {other:?}"),
        }
    }

    #[test]
    fn survey_and_worktree_share_common_dir_while_nested_repositories_are_separate() {
        let (temp, repo_a) = repo("a");
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("b")).unwrap();
        std::fs::create_dir_all(root.join("plain")).unwrap();
        // A nested repository inside A has its own common directory.
        let nested = repo_a.join("nested");
        std::fs::create_dir(&nested).unwrap();
        git(&nested, &["init", "-q", "-b", "main"]);
        git(&nested, &["config", "user.email", "test@example.invalid"]);
        git(&nested, &["config", "user.name", "Test"]);
        std::fs::write(nested.join("n.txt"), "n\n").unwrap();
        git(&nested, &["add", "."]);
        git(&nested, &["commit", "-q", "-m", "nested"]);
        // A linked worktree has a `.git` file and the same common directory.
        let worktree = root.join("wt");
        git(
            &repo_a,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "wt-branch",
                worktree.to_str().unwrap(),
            ],
        );
        let from_target = discover_target(&repo_a, &options(&root));
        let repo = from_target.repository().unwrap().clone();
        let from_worktree = discover_target(&worktree, &options(&root));
        let linked = from_worktree.repository().unwrap();
        assert_eq!(repo.common_dir, linked.common_dir);
        assert_ne!(repo.root, linked.root);
        assert!(worktree.join(".git").is_file());
        let nested_discovery = discover_target(&nested, &options(&root));
        assert_ne!(
            repo.common_dir,
            nested_discovery.repository().unwrap().common_dir
        );

        let scanned = scan_repositories(&root, &options(&root)).unwrap();
        let common: Vec<_> = scanned
            .discoveries
            .iter()
            .filter_map(Discovery::repository)
            .map(|repo| repo.common_dir.clone())
            .collect();
        assert_eq!(
            common
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            common.len(),
            "duplicate common directories must be collapsed"
        );
        assert_eq!(common.len(), 2); // a (shared by its worktree) and nested
    }

    #[test]
    fn configured_target_branch_wins_and_detached_head_requires_a_choice() {
        let (_temp, root) = repo("repo");
        git(&root, &["switch", "-q", "-c", "develop"]);
        let discovery = discover_target(
            &root,
            &DiscoveryOptions {
                authorized_roots: vec![root.clone()],
                ..DiscoveryOptions::default()
            },
        );
        let repo = discovery.repository().unwrap();
        assert_eq!(repo.current_branch.as_deref(), Some("develop"));
        assert_eq!(select_target_branch(repo, None).unwrap(), "develop");
        assert_eq!(
            select_target_branch(repo, Some("main")).unwrap(),
            "main",
            "a configured branch wins and must exist"
        );
        assert_eq!(
            select_target_branch(repo, Some("missing")),
            Err(BranchSelectionError::Missing("missing".into()))
        );
        git(&root, &["checkout", "-q", "--detach"]);
        let detached = discover_target(
            &root,
            &DiscoveryOptions {
                authorized_roots: vec![root.clone()],
                ..DiscoveryOptions::default()
            },
        );
        let detached = detached.repository().unwrap();
        assert!(detached.detached);
        assert_eq!(
            select_target_branch(detached, None),
            Err(BranchSelectionError::Detached)
        );
        assert!(validate_branch_name("--force").is_err());
        assert!(validate_branch_name("bad..name").is_err());
        assert!(validate_branch_name("release/1.0").is_ok());
    }

    #[test]
    fn unborn_and_in_progress_repositories_require_setup() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let unborn = root.join("unborn");
        std::fs::create_dir(&unborn).unwrap();
        git(&unborn, &["init", "-q", "-b", "main"]);
        match discover_target(&unborn, &options(&root)) {
            Discovery::GitSetupRequired { reason, .. } => {
                assert!(reason.contains("no commits"), "{reason}")
            }
            other => panic!("expected setup requirement, got {other:?}"),
        }

        let (_repo_temp, repository) = repo("in-progress");
        let merge_head = repository.join(".git/MERGE_HEAD");
        std::fs::write(&merge_head, "0000000000000000000000000000000000000000\n").unwrap();
        match discover_target(
            &repository,
            &DiscoveryOptions {
                authorized_roots: vec![repository.clone()],
                ..DiscoveryOptions::default()
            },
        ) {
            Discovery::GitSetupRequired { reason, .. } => assert!(reason.contains("merge")),
            other => panic!("expected merge setup requirement, got {other:?}"),
        }
    }

    #[test]
    fn source_state_records_modified_and_untracked_files_without_changing_them() {
        let (_temp, root) = repo("source");
        std::fs::write(root.join("file.txt"), "edited\n").unwrap();
        std::fs::write(root.join("new.txt"), "new\n").unwrap();
        let state = source_state(&root).unwrap();
        assert!(state.dirty);
        assert_eq!(state.branch.as_deref(), Some("main"));
        assert!(state.entries.iter().any(|entry| entry.contains("file.txt")));
        assert!(state.entries.iter().any(|entry| entry.contains("new.txt")));
        assert_eq!(
            std::fs::read_to_string(root.join("file.txt")).unwrap(),
            "edited\n"
        );
    }

    #[test]
    fn scanning_is_bounded_and_skips_dependency_directories() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("plain-a")).unwrap();
        std::fs::create_dir(root.join("plain-b")).unwrap();
        let hidden = root.join("node_modules/hidden");
        std::fs::create_dir_all(&hidden).unwrap();
        git(&hidden, &["init", "-q", "-b", "main"]);
        git(&hidden, &["config", "user.email", "test@example.invalid"]);
        git(&hidden, &["config", "user.name", "Test"]);
        git(&hidden, &["commit", "-q", "--allow-empty", "-m", "hidden"]);
        let mut options = options(&root);
        options.max_entries = 1;
        let outcome = scan_repositories(&root, &options).unwrap();
        assert!(outcome.truncated);
        assert!(outcome.discoveries.iter().all(|discovery| !discovery
            .target()
            .to_string_lossy()
            .contains("node_modules")));
    }

    #[test]
    fn submodule_checkout_records_the_superproject() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let parent = root.join("parent");
        let child = root.join("child-source");
        std::fs::create_dir_all(&child).unwrap();
        git(&child, &["init", "-q", "-b", "main"]);
        git(&child, &["config", "user.email", "test@example.invalid"]);
        git(&child, &["config", "user.name", "Test"]);
        std::fs::write(child.join("c.txt"), "c\n").unwrap();
        git(&child, &["add", "."]);
        git(&child, &["commit", "-q", "-m", "child"]);
        std::fs::create_dir_all(&parent).unwrap();
        git(&parent, &["init", "-q", "-b", "main"]);
        git(&parent, &["config", "user.email", "test@example.invalid"]);
        git(&parent, &["config", "user.name", "Test"]);
        git(&parent, &["commit", "-q", "--allow-empty", "-m", "parent"]);
        let added = Command::new("git")
            .current_dir(&parent)
            .args([
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                child.to_str().unwrap(),
                "sub",
            ])
            .output()
            .unwrap();
        assert!(
            added.status.success(),
            "{}",
            String::from_utf8_lossy(&added.stderr)
        );
        let discovery = discover_target(&parent.join("sub"), &options(&root));
        let repo = discovery.repository().unwrap();
        assert_eq!(repo.superproject.as_deref(), Some(parent.as_path()));
    }
}
