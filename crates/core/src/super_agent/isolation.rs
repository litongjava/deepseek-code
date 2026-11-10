//! Task execution environments: Git worktrees and non-Git snapshots.
//!
//! A Git target is isolated through a task branch and a managed worktree. A
//! non-Git target is modified in place, so it is snapshotted first and guarded
//! by a path lock. Restoring a snapshot refuses to overwrite changes the user
//! made after the task started.
use super::{
    discovery::{select_target_branch, source_state, Discovery, GitRepository},
    model::{NonGitStatus, NonGitTarget, RepoTarget, VerificationStatus},
    store::TaskStore,
};
use crate::worktree::session::{prepare_task_worktree, TaskWorktree};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write,
    path::{Component, Path, PathBuf},
};

pub const MAX_SNAPSHOT_ENTRIES: usize = 10_000;
pub const MAX_SNAPSHOT_BYTES: u64 = 256 * 1024 * 1024;
const SNAPSHOT_SKIP: [&str; 8] = [
    ".git",
    "node_modules",
    "target",
    "dist",
    "build",
    ".venv",
    "venv",
    "vendor",
];

#[derive(Debug, Clone)]
pub struct TargetPlan {
    pub discovery: Discovery,
    /// Overrides the repository's current branch. The discovery layer verifies
    /// that a configured branch exists.
    pub target_branch: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SnapshotKind {
    File,
    Directory,
    Symlink,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotEntry {
    pub path: String,
    pub kind: SnapshotKind,
    pub hash: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_target: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotManifest {
    pub source: PathBuf,
    pub created_at: String,
    pub entries: Vec<SnapshotEntry>,
    pub skipped: Vec<String>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SnapshotChangeKind {
    Added,
    Removed,
    Modified,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotChange {
    pub path: String,
    pub kind: SnapshotChangeKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotDiff {
    pub changes: Vec<SnapshotChange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreReport {
    pub restored: usize,
    pub removed: usize,
}

#[derive(Clone)]
pub struct PreparedEnvironment {
    pub cwd: PathBuf,
    pub workspace_root: Option<PathBuf>,
    pub repositories: Vec<RepoTarget>,
    pub non_git_targets: Vec<NonGitTarget>,
    worktrees: Vec<TaskWorktree>,
    locks: Vec<(String, String, String)>,
    store: Option<TaskStore>,
}

impl std::fmt::Debug for PreparedEnvironment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedEnvironment")
            .field("cwd", &self.cwd)
            .field("workspace_root", &self.workspace_root)
            .field("repositories", &self.repositories)
            .field("non_git_targets", &self.non_git_targets)
            .finish()
    }
}

impl PreparedEnvironment {
    /// Removes Git worktrees without force and releases path locks. Snapshots
    /// and the task data directory are intentionally retained.
    pub fn cleanup(&self) {
        for worktree in &self.worktrees {
            worktree.cleanup();
        }
        if let Some(store) = &self.store {
            for (key, task, operation) in &self.locks {
                let _ = store.unlock(key, task, operation);
            }
        }
    }
}

struct EnvironmentGuard {
    worktrees: Vec<TaskWorktree>,
    locks: Vec<(String, String, String)>,
    store: Option<TaskStore>,
    committed: bool,
}

impl EnvironmentGuard {
    fn commit(
        mut self,
    ) -> (
        Vec<TaskWorktree>,
        Vec<(String, String, String)>,
        Option<TaskStore>,
    ) {
        self.committed = true;
        (
            std::mem::take(&mut self.worktrees),
            std::mem::take(&mut self.locks),
            self.store.take(),
        )
    }
}

impl Drop for EnvironmentGuard {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        for worktree in &self.worktrees {
            worktree.cleanup();
        }
        if let Some(store) = &self.store {
            for (key, task, operation) in &self.locks {
                let _ = store.unlock(key, task, operation);
            }
        }
    }
}

/// Prepares every target and records the resulting environment when a store is
/// supplied. It is deliberately not idempotent: a task with existing targets
/// must go through the recovery path instead of silently creating a second
/// worktree or snapshot.
pub fn prepare_environment(
    task_id: &str,
    data_dir: &Path,
    plans: &[TargetPlan],
    store: Option<&TaskStore>,
    operation_prefix: &str,
) -> Result<PreparedEnvironment> {
    ensure!(!plans.is_empty(), "A task needs at least one target");
    super::store::validate_task_id(task_id).map_err(|error| anyhow::anyhow!(error.to_string()))?;
    let task_dir = data_dir.join("super-agent").join(task_id);
    if let Some(store) = store {
        if !store
            .repos(task_id)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?
            .is_empty()
            || !store
                .non_git_targets(task_id)
                .map_err(|error| anyhow::anyhow!(error.to_string()))?
                .is_empty()
        {
            anyhow::bail!("Task environment is already recorded; use the recovery path");
        }
    }
    let mut guard = EnvironmentGuard {
        worktrees: Vec::new(),
        locks: Vec::new(),
        store: store.cloned(),
        committed: false,
    };
    let mut repositories = Vec::new();
    let mut non_git_targets = Vec::new();
    let mut git_by_common: BTreeMap<String, (String, GitRepository)> = BTreeMap::new();
    let mut used_ids: BTreeMap<String, u32> = BTreeMap::new();

    for plan in plans {
        match &plan.discovery {
            Discovery::GitRepository(repo) => {
                let common_key = repo.common_dir.to_string_lossy().into_owned();
                let branch = match plan.target_branch.as_deref() {
                    Some(branch) => branch.to_owned(),
                    None => select_target_branch(repo, None)
                        .map_err(|error| anyhow::anyhow!(error.to_string()))?,
                };
                if let Some((existing_branch, _)) = git_by_common.get(&common_key) {
                    ensure!(
                        existing_branch == &branch,
                        "The same repository has conflicting target branches"
                    );
                    continue;
                }
                let worktree = prepare_task_worktree(&repo.root, Some(&branch), task_id)?;
                let working_path = worktree_working_path(&worktree, &repo.target_path)?;
                let state = source_state(&repo.root)?;
                let repository_id = unique_id(&mut used_ids, &repo.root);
                let target = RepoTarget {
                    task_id: task_id.into(),
                    repository_id,
                    repo_root: repo.root.clone(),
                    git_common_dir: repo.common_dir.clone(),
                    target_path: repo.target_path.clone(),
                    target_branch: branch.clone(),
                    task_branch: worktree.task_branch.clone(),
                    worktree_path: worktree.worktree_root.clone(),
                    working_path: working_path.clone(),
                    base_commit: worktree.base_commit.clone(),
                    submitted_commit: None,
                    reviewed_commit: None,
                    integrated_commit: None,
                    integration_status: Default::default(),
                    verification_status: VerificationStatus::Pending,
                    source_state: state.entries,
                };
                git_by_common.insert(common_key, (branch, repo.clone()));
                guard.worktrees.push(worktree);
                repositories.push(target);
            }
            Discovery::NonGit { target } => {
                let path = target.canonicalize().with_context(|| {
                    format!("Non-Git target is unavailable: {}", target.display())
                })?;
                ensure!(path.is_dir(), "Non-Git target must be a directory");
                let index = non_git_targets.len() + 1;
                let lock_key = format!("path:{}", path.display());
                let lock_operation = format!("{operation_prefix}:path:{index}");
                if let Some(store) = store {
                    // A deterministic operation that was released earlier must
                    // not masquerade as ownership; acquire with a fresh one.
                    let mut operation = lock_operation.clone();
                    loop {
                        match store
                            .try_lock(&operation, &lock_key, task_id)
                            .map_err(|error| anyhow::anyhow!(error.to_string()))?
                        {
                            super::store::LockOutcome::Acquired
                            | super::store::LockOutcome::AlreadyOwned => break,
                            super::store::LockOutcome::Released => {
                                operation = format!("{lock_operation}:{}", uuid::Uuid::new_v4());
                            }
                        }
                    }
                    guard
                        .locks
                        .push((lock_key.clone(), task_id.into(), operation));
                }
                let snapshot_dir = task_dir.join("snapshots").join(index.to_string());
                let manifest = match load_snapshot(&snapshot_dir) {
                    Ok(manifest) => {
                        ensure!(
                            manifest.source == path,
                            "Existing snapshot belongs to a different path"
                        );
                        manifest
                    }
                    Err(_) if !snapshot_dir.exists() => create_snapshot(&path, &snapshot_dir)?,
                    Err(error) => return Err(error),
                };
                let record = NonGitTarget {
                    task_id: task_id.into(),
                    path: path.clone(),
                    snapshot_dir,
                    snapshot_manifest_json: serde_json::to_string(&manifest)?,
                    diff_ref: None,
                    status: NonGitStatus::Prepared,
                    lock_key,
                };
                non_git_targets.push(record);
            }
            Discovery::GitSetupRequired {
                target,
                reason,
                root,
                ..
            } => anyhow::bail!(
                "Repository setup is required for {} ({}): {reason}",
                target.display(),
                root.as_deref().unwrap_or(target).display()
            ),
            Discovery::DetectionFailed { target, error } => anyhow::bail!(
                "Repository detection failed for {}: {error}",
                target.display()
            ),
        }
    }
    ensure!(
        !repositories.is_empty() || !non_git_targets.is_empty(),
        "No usable task targets were found"
    );

    let single = repositories.len() + non_git_targets.len() == 1;
    let (cwd, workspace_root) = if single {
        let cwd = repositories
            .first()
            .map(|repo| repo.working_path.clone())
            .or_else(|| non_git_targets.first().map(|target| target.path.clone()))
            .context("Task target disappeared")?;
        (cwd, None)
    } else {
        let workspace = task_dir.join("workspace");
        std::fs::create_dir_all(&workspace)?;
        let mut names = BTreeSet::new();
        for repo in &repositories {
            let name = link_name(&mut names, &repo.repository_id);
            link_directory(&repo.worktree_path, &workspace.join(name))?;
        }
        for target in &non_git_targets {
            let base = target
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("target");
            let name = link_name(&mut names, base);
            link_directory(&target.path, &workspace.join(name))?;
        }
        (workspace.clone(), Some(workspace))
    };
    if let Some(store) = store {
        store
            .record_environment(&repositories, &non_git_targets)
            .map_err(|error| anyhow::anyhow!(error.to_string()))?;
    }
    let (worktrees, locks, store) = guard.commit();
    Ok(PreparedEnvironment {
        cwd,
        workspace_root,
        repositories,
        non_git_targets,
        worktrees,
        locks,
        store,
    })
}

fn unique_id(used: &mut BTreeMap<String, u32>, repo_root: &Path) -> String {
    let name = repo_root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("repo");
    let base: String = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    let base = if base.is_empty() { "repo".into() } else { base };
    let count = used.entry(base.clone()).or_insert(0);
    *count += 1;
    if *count == 1 {
        base
    } else {
        format!("{base}-{}", *count)
    }
}

fn link_name(names: &mut BTreeSet<String>, base: &str) -> String {
    let base: String = base
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    let base = if base.is_empty() {
        "target".to_owned()
    } else {
        base
    };
    let mut name = base.clone();
    let mut index = 2;
    while !names.insert(name.clone()) {
        name = format!("{base}-{index}");
        index += 1;
    }
    name
}

fn worktree_working_path(worktree: &TaskWorktree, target: &Path) -> Result<PathBuf> {
    let canonical_target = target.canonicalize().ok();
    let mut relative = match canonical_target {
        Some(target) => target
            .strip_prefix(&worktree.repo_root)
            .map(Path::to_path_buf)
            .unwrap_or_default(),
        None => PathBuf::new(),
    };
    loop {
        let candidate = if relative.as_os_str().is_empty() {
            worktree.worktree_root.clone()
        } else {
            worktree.worktree_root.join(&relative)
        };
        if candidate.is_dir() {
            return Ok(candidate);
        }
        if !relative.pop() {
            return Ok(worktree.worktree_root.clone());
        }
    }
}

fn link_directory(target: &Path, link: &Path) -> Result<()> {
    if link.symlink_metadata().is_ok() {
        let actual = link
            .canonicalize()
            .with_context(|| format!("Existing workspace link is broken: {}", link.display()))?;
        ensure!(
            actual == target.canonicalize()?,
            "Workspace entry already points somewhere else: {}",
            link.display()
        );
        return Ok(());
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link)
        .with_context(|| format!("Linking {} into the task workspace", target.display()))?;
    #[cfg(windows)]
    {
        let output = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .context("Creating a task workspace junction")?;
        ensure!(
            output.status.success(),
            "Creating a task workspace junction failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

fn walk(source: &Path, copy_to: Option<&Path>) -> Result<(Vec<SnapshotEntry>, Vec<String>, bool)> {
    let mut entries = Vec::new();
    let mut skipped = Vec::new();
    let mut truncated = false;
    let mut total_bytes = 0u64;
    let mut stack = vec![source.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let mut children = std::fs::read_dir(&directory)
            .with_context(|| format!("Reading {}", directory.display()))?
            .collect::<std::io::Result<Vec<_>>>()?;
        children.sort_by_key(|entry| entry.file_name());
        for child in children {
            let path = child.path();
            let name = child.file_name().to_string_lossy().into_owned();
            if SNAPSHOT_SKIP.contains(&name.as_str()) {
                skipped.push(relative_string(source, &path));
                continue;
            }
            let metadata = std::fs::symlink_metadata(&path)?;
            let file_type = metadata.file_type();
            let relative = match path.strip_prefix(source) {
                Ok(relative) => relative,
                Err(_) => continue,
            };
            let Some(relative_string) = relative.to_str().map(str::to_owned) else {
                skipped.push(format!("non-UTF-8 path {}", relative.display()));
                continue;
            };
            if entries.len() >= MAX_SNAPSHOT_ENTRIES {
                truncated = true;
                break;
            }
            if file_type.is_symlink() {
                let link_target = std::fs::read_link(&path)?;
                let text = link_target.to_string_lossy().into_owned();
                entries.push(SnapshotEntry {
                    path: relative_string.clone(),
                    kind: SnapshotKind::Symlink,
                    hash: hash_bytes(text.as_bytes()),
                    size: 0,
                    link_target: Some(text),
                });
                if let Some(destination) = copy_to {
                    copy_symlink(&link_target, &destination.join(&relative_string))?;
                }
            } else if file_type.is_dir() {
                entries.push(SnapshotEntry {
                    path: relative_string.clone(),
                    kind: SnapshotKind::Directory,
                    hash: String::new(),
                    size: 0,
                    link_target: None,
                });
                if let Some(destination) = copy_to {
                    std::fs::create_dir_all(destination.join(&relative_string))?;
                }
                stack.push(path);
            } else if file_type.is_file() {
                let size = metadata.len();
                total_bytes = total_bytes.saturating_add(size);
                if total_bytes > MAX_SNAPSHOT_BYTES {
                    truncated = true;
                    break;
                }
                let hash = hash_file(&path)?;
                entries.push(SnapshotEntry {
                    path: relative_string.clone(),
                    kind: SnapshotKind::File,
                    hash,
                    size,
                    link_target: None,
                });
                if let Some(destination) = copy_to {
                    let destination = destination.join(&relative_string);
                    if let Some(parent) = destination.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::copy(&path, &destination)?;
                }
            } else {
                skipped.push(relative_string);
            }
        }
        if truncated {
            break;
        }
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    skipped.sort();
    Ok((entries, skipped, truncated))
}

fn relative_string(source: &Path, path: &Path) -> String {
    path.strip_prefix(source)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

#[cfg(unix)]
fn copy_symlink(target: &Path, destination: &Path) -> Result<()> {
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(destination);
    std::os::unix::fs::symlink(target, destination)?;
    Ok(())
}

#[cfg(windows)]
fn copy_symlink(target: &Path, destination: &Path) -> Result<()> {
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(destination);
    // A non-privileged Windows process may not be allowed to create links.
    // The manifest still records the link and restore can retry.
    let target_is_dir = target.is_dir();
    let result = if target_is_dir {
        std::os::windows::fs::symlink_dir(target, destination)
    } else {
        std::os::windows::fs::symlink_file(target, destination)
    };
    let _ = result;
    Ok(())
}

fn hash_file(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hex(hasher.finalize()))
}

fn hash_bytes(bytes: &[u8]) -> String {
    hex(Sha256::digest(bytes))
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub fn create_snapshot(source: &Path, snapshot_dir: &Path) -> Result<SnapshotManifest> {
    let source = source.canonicalize()?;
    ensure!(source.is_dir(), "Snapshot source must be a directory");
    ensure!(
        !snapshot_dir.exists(),
        "Snapshot directory already exists: {}",
        snapshot_dir.display()
    );
    std::fs::create_dir_all(snapshot_dir)?;
    let result = (|| -> Result<SnapshotManifest> {
        let (entries, skipped, truncated) = walk(&source, Some(snapshot_dir))?;
        ensure!(
            !truncated,
            "Target exceeds snapshot limits ({} entries or {} bytes)",
            MAX_SNAPSHOT_ENTRIES,
            MAX_SNAPSHOT_BYTES
        );
        let manifest = SnapshotManifest {
            source: source.clone(),
            created_at: chrono::Utc::now().to_rfc3339(),
            entries,
            skipped,
            truncated,
        };
        write_manifest(snapshot_dir, &manifest)?;
        Ok(manifest)
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(snapshot_dir);
    }
    result
}

/// Reads the stored snapshot and verifies that every protected payload entry
/// is present with matching content. It never modifies the source tree, and it
/// reports entries that the snapshot intentionally skipped instead of claiming
/// they are protected.
pub fn verify_snapshot(
    snapshot_dir: &Path,
    expected_source: &Path,
) -> super::model::NonGitVerification {
    let mut verification = super::model::NonGitVerification::default();
    let manifest = match load_snapshot(snapshot_dir) {
        Ok(manifest) => manifest,
        Err(error) => {
            verification.error = Some(format!("{error:#}"));
            return verification;
        }
    };
    verification.valid = true;
    verification.source_matches = manifest
        .source
        .canonicalize()
        .ok()
        .zip(expected_source.canonicalize().ok())
        .is_some_and(|(left, right)| left == right);
    verification.skipped = manifest.skipped.clone();
    if !verification.source_matches {
        verification.error = Some("snapshot source does not match the target".into());
        return verification;
    }
    if manifest.truncated {
        verification.error = Some("snapshot was truncated".into());
        return verification;
    }
    let payload = snapshot_dir.join("payload");
    for entry in &manifest.entries {
        if !super::discovery::is_plain_relative_path(&entry.path) {
            verification.error = Some(format!("snapshot entry {} is not a plain path", entry.path));
            return verification;
        }
        match entry.kind {
            SnapshotKind::Directory => {}
            SnapshotKind::File => {
                let path = payload.join(&entry.path);
                match hash_file(&path) {
                    Ok(hash) if hash == entry.hash => {}
                    Ok(_) => {
                        verification.error =
                            Some(format!("payload hash mismatch for {}", entry.path));
                        return verification;
                    }
                    Err(error) => {
                        verification.error = Some(format!(
                            "payload for {} is unavailable: {error}",
                            entry.path
                        ));
                        return verification;
                    }
                }
            }
            SnapshotKind::Symlink => {
                let matches = entry
                    .link_target
                    .as_deref()
                    .is_some_and(|target| hash_bytes(target.as_bytes()) == entry.hash);
                if !matches {
                    verification.error =
                        Some(format!("symlink payload mismatch for {}", entry.path));
                    return verification;
                }
            }
        }
    }
    verification.payload_verified = true;
    verification
}

pub fn load_snapshot(snapshot_dir: &Path) -> Result<SnapshotManifest> {
    let path = snapshot_dir.join("manifest.json");
    let bytes = std::fs::read(&path)
        .with_context(|| format!("Reading snapshot manifest {}", path.display()))?;
    serde_json::from_slice(&bytes).context("Parsing snapshot manifest")
}

fn write_manifest(snapshot_dir: &Path, manifest: &SnapshotManifest) -> Result<()> {
    let mut temporary = tempfile::NamedTempFile::new_in(snapshot_dir)?;
    serde_json::to_writer(&mut temporary, manifest)?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(snapshot_dir.join("manifest.json"))
        .map_err(|error| error.error)?;
    #[cfg(unix)]
    std::fs::File::open(snapshot_dir)?.sync_all()?;
    Ok(())
}

pub fn current_manifest(source: &Path) -> Result<SnapshotManifest> {
    let source = source.canonicalize()?;
    ensure!(source.is_dir(), "Snapshot source must be a directory");
    let (entries, skipped, truncated) = walk(&source, None)?;
    Ok(SnapshotManifest {
        source,
        created_at: String::new(),
        entries,
        skipped,
        truncated,
    })
}

pub fn diff_snapshot(before: &SnapshotManifest, current: &SnapshotManifest) -> SnapshotDiff {
    let before: BTreeMap<_, _> = before
        .entries
        .iter()
        .map(|entry| (entry.path.clone(), entry))
        .collect();
    let current: BTreeMap<_, _> = current
        .entries
        .iter()
        .map(|entry| (entry.path.clone(), entry))
        .collect();
    let mut changes = Vec::new();
    for (path, entry) in &current {
        match before.get(path) {
            None => changes.push(SnapshotChange {
                path: path.clone(),
                kind: SnapshotChangeKind::Added,
            }),
            Some(previous)
                if previous.kind != entry.kind
                    || previous.hash != entry.hash
                    || previous.link_target != entry.link_target =>
            {
                changes.push(SnapshotChange {
                    path: path.clone(),
                    kind: SnapshotChangeKind::Modified,
                })
            }
            Some(_) => {}
        }
    }
    for path in before.keys() {
        if !current.contains_key(path) {
            changes.push(SnapshotChange {
                path: path.clone(),
                kind: SnapshotChangeKind::Removed,
            });
        }
    }
    changes.sort_by(|left, right| left.path.cmp(&right.path));
    SnapshotDiff { changes }
}

fn same_content(left: &SnapshotManifest, right: &SnapshotManifest) -> bool {
    left.entries == right.entries
        && left.skipped == right.skipped
        && left.truncated == right.truncated
}

/// Restores the snapshot only if the current tree still equals the tree the
/// task is expected to have produced. Any later user edit makes this refuse
/// instead of overwriting newer content.
pub fn restore_snapshot(
    manifest: &SnapshotManifest,
    snapshot_dir: &Path,
    source: &Path,
    expected_after: &SnapshotManifest,
) -> Result<RestoreReport> {
    ensure!(
        !manifest.truncated && !expected_after.truncated,
        "Refusing to restore from a truncated snapshot"
    );
    let source = source.canonicalize()?;
    ensure!(
        manifest.source == source,
        "Snapshot belongs to a different directory"
    );
    let current = current_manifest(&source)?;
    ensure!(
        same_content(&current, expected_after),
        "The target changed after the task modified it; refusing to overwrite user changes"
    );
    if same_content(&current, manifest) {
        return Ok(RestoreReport {
            restored: 0,
            removed: 0,
        });
    }
    let current_by_path: BTreeMap<_, _> = current
        .entries
        .iter()
        .map(|entry| (entry.path.clone(), entry))
        .collect();
    let before_by_path: BTreeMap<_, _> = manifest
        .entries
        .iter()
        .map(|entry| (entry.path.clone(), entry))
        .collect();
    let mut removed = 0usize;
    // Remove files created after the snapshot, deepest first.
    let mut removals: Vec<&SnapshotEntry> = expected_after
        .entries
        .iter()
        .filter(|entry| !before_by_path.contains_key(&entry.path))
        .collect();
    removals.sort_by(|left, right| right.path.cmp(&left.path));
    for entry in removals {
        let target = safe_target(&source, &entry.path)?;
        if std::fs::symlink_metadata(&target).is_ok() {
            remove_entry(&target)?;
            removed += 1;
        }
    }
    let mut restored = 0usize;
    for entry in &manifest.entries {
        let target = safe_target(&source, &entry.path)?;
        match entry.kind {
            SnapshotKind::Directory => {
                if !target.is_dir() {
                    if std::fs::symlink_metadata(&target).is_ok() {
                        remove_entry(&target)?;
                    }
                    std::fs::create_dir_all(&target)?;
                }
            }
            SnapshotKind::File => {
                if current_by_path
                    .get(&entry.path)
                    .is_some_and(|current| current.hash == entry.hash)
                {
                    continue;
                }
                let snapshot_path = snapshot_dir.join(&entry.path);
                let bytes = std::fs::read(&snapshot_path)
                    .with_context(|| format!("Snapshot copy is missing for {}", entry.path))?;
                ensure!(
                    hash_bytes(&bytes) == entry.hash,
                    "Snapshot copy is corrupt for {}",
                    entry.path
                );
                if std::fs::symlink_metadata(&target)
                    .map(|metadata| metadata.is_dir())
                    .unwrap_or(false)
                {
                    remove_entry(&target)?;
                }
                write_atomic(&target, &bytes)?;
                restored += 1;
            }
            SnapshotKind::Symlink => {
                let Some(link_target) = entry.link_target.as_deref() else {
                    anyhow::bail!("Snapshot symlink {} has no target", entry.path);
                };
                if current_by_path
                    .get(&entry.path)
                    .is_some_and(|current| current.link_target.as_deref() == Some(link_target))
                {
                    continue;
                }
                if std::fs::symlink_metadata(&target).is_ok() {
                    remove_entry(&target)?;
                }
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                #[cfg(unix)]
                std::os::unix::fs::symlink(link_target, &target)?;
                #[cfg(windows)]
                {
                    let _ = std::os::windows::fs::symlink_file(link_target, &target);
                }
                restored += 1;
            }
        }
    }
    // Remove now-empty directories that only existed after the snapshot.
    let mut directories: Vec<&SnapshotEntry> = expected_after
        .entries
        .iter()
        .filter(|entry| {
            entry.kind == SnapshotKind::Directory && !before_by_path.contains_key(&entry.path)
        })
        .collect();
    directories.sort_by(|left, right| right.path.cmp(&left.path));
    for entry in directories {
        if let Ok(target) = safe_target(&source, &entry.path) {
            let _ = std::fs::remove_dir(target);
        }
    }
    Ok(RestoreReport { restored, removed })
}

fn safe_target(source: &Path, relative: &str) -> Result<PathBuf> {
    let relative = Path::new(relative);
    ensure!(
        !relative.as_os_str().is_empty()
            && relative
                .components()
                .all(|component| matches!(component, Component::Normal(_))),
        "Snapshot path is not a plain relative path"
    );
    let mut current = source.to_path_buf();
    let components: Vec<_> = relative.components().collect();
    for (index, component) in components.iter().enumerate() {
        current.push(component);
        if index + 1 < components.len() {
            match std::fs::symlink_metadata(&current) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    anyhow::bail!("Snapshot path escapes through a symbolic link")
                }
                Ok(metadata) if !metadata.is_dir() => {
                    anyhow::bail!("Snapshot path crosses a non-directory")
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    Ok(current)
}

fn remove_entry(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        std::fs::remove_dir(path)?;
    } else {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("Restored file has no parent")?;
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(bytes)?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git(cwd: &Path, args: &[&str]) {
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
    }

    fn repo() -> (tempfile::TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("repo");
        std::fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q", "-b", "main"]);
        git(&root, &["config", "user.email", "test@example.invalid"]);
        git(&root, &["config", "user.name", "Test"]);
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.txt"), "main\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", "initial"]);
        git(&root, &["switch", "-q", "-c", "develop"]);
        std::fs::write(root.join("src/main.txt"), "develop\n").unwrap();
        git(&root, &["commit", "-q", "-am", "develop"]);
        git(&root, &["switch", "-q", "main"]);
        (temp, root.canonicalize().unwrap())
    }

    fn options(root: &Path) -> super::super::discovery::DiscoveryOptions {
        super::super::discovery::DiscoveryOptions {
            authorized_roots: vec![root.to_path_buf()],
            ..Default::default()
        }
    }

    #[test]
    fn git_worktree_uses_the_selected_non_main_branch_and_agent_branch() {
        let (temp, root) = repo();
        let discovery = super::super::discovery::discover_target(&root, &options(temp.path()));
        let plan = TargetPlan {
            discovery,
            target_branch: Some("develop".into()),
        };
        let environment =
            prepare_environment("task-001", temp.path(), &[plan], None, "isolation:task-001")
                .unwrap();
        assert_eq!(environment.repositories.len(), 1);
        let record = &environment.repositories[0];
        assert_eq!(record.target_branch, "develop");
        assert_eq!(record.task_branch, "agent/task-001");
        assert!(record.worktree_path.ends_with("desktop-worktrees/task-001"));
        assert_eq!(
            std::fs::read_to_string(environment.cwd.join("src/main.txt")).unwrap(),
            "develop\n"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("src/main.txt")).unwrap(),
            "main\n",
            "the source worktree must not be touched"
        );
        assert_eq!(record.working_path, environment.cwd);
        environment.cleanup();
        assert!(!record.worktree_path.exists());
    }

    #[test]
    fn multi_repository_workspace_links_each_worktree() {
        let (temp, first) = repo();
        let second = temp.path().join("second");
        std::fs::create_dir(&second).unwrap();
        git(&second, &["init", "-q", "-b", "main"]);
        git(&second, &["config", "user.email", "test@example.invalid"]);
        git(&second, &["config", "user.name", "Test"]);
        std::fs::write(second.join("docs.txt"), "docs\n").unwrap();
        git(&second, &["add", "."]);
        git(&second, &["commit", "-q", "-m", "docs"]);
        let second = second.canonicalize().unwrap();
        let plans = vec![
            TargetPlan {
                discovery: super::super::discovery::discover_target(&first, &options(temp.path())),
                target_branch: None,
            },
            TargetPlan {
                discovery: super::super::discovery::discover_target(&second, &options(temp.path())),
                target_branch: None,
            },
        ];
        let environment =
            prepare_environment("task-002", temp.path(), &plans, None, "isolation:task-002")
                .unwrap();
        assert_eq!(environment.repositories.len(), 2);
        let workspace = environment.workspace_root.as_ref().unwrap();
        assert_eq!(environment.cwd, *workspace);
        #[cfg(unix)]
        {
            let links = std::fs::read_dir(workspace)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect::<Vec<_>>();
            assert_eq!(links.len(), 2);
            for link in links {
                assert!(std::fs::symlink_metadata(&link)
                    .unwrap()
                    .file_type()
                    .is_symlink());
            }
            assert!(workspace.join("repo/src/main.txt").is_file());
            assert!(workspace.join("second/docs.txt").is_file());
        }
        environment.cleanup();
    }

    #[test]
    fn non_git_target_is_snapshotted_without_creating_git() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("scripts");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("run.sh"), "echo old\n").unwrap();
        std::fs::create_dir(target.join("nested")).unwrap();
        std::fs::write(target.join("nested/keep.txt"), "keep\n").unwrap();
        let plan = TargetPlan {
            discovery: super::super::discovery::discover_target(&target, &options(temp.path())),
            target_branch: None,
        };
        let environment =
            prepare_environment("task-003", temp.path(), &[plan], None, "isolation:task-003")
                .unwrap();
        assert_eq!(environment.cwd, target.canonicalize().unwrap());
        assert_eq!(environment.non_git_targets.len(), 1);
        assert!(environment.non_git_targets[0]
            .snapshot_dir
            .join("manifest.json")
            .is_file());
        assert!(!target.join(".git").exists());
        environment.cleanup();
    }

    #[test]
    fn snapshot_diff_and_restore_round_trip() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("a.txt"), "a\n").unwrap();
        std::fs::write(source.join("b.txt"), "b\n").unwrap();
        let snapshot_dir = temp.path().join("snapshots/1");
        let manifest = create_snapshot(&source, &snapshot_dir).unwrap();
        std::fs::write(source.join("a.txt"), "changed\n").unwrap();
        std::fs::remove_file(source.join("b.txt")).unwrap();
        std::fs::write(source.join("new.txt"), "new\n").unwrap();
        std::fs::create_dir(source.join("newdir")).unwrap();
        let changed = current_manifest(&source).unwrap();
        let diff = diff_snapshot(&manifest, &changed);
        assert!(diff.changes.iter().any(|change| {
            change.path == "a.txt" && change.kind == SnapshotChangeKind::Modified
        }));
        assert!(diff.changes.iter().any(|change| {
            change.path == "b.txt" && change.kind == SnapshotChangeKind::Removed
        }));
        assert!(diff.changes.iter().any(|change| {
            change.path == "new.txt" && change.kind == SnapshotChangeKind::Added
        }));
        let report = restore_snapshot(&manifest, &snapshot_dir, &source, &changed).unwrap();
        assert!(report.restored >= 1);
        assert!(report.removed >= 1);
        assert_eq!(
            std::fs::read_to_string(source.join("a.txt")).unwrap(),
            "a\n"
        );
        assert_eq!(
            std::fs::read_to_string(source.join("b.txt")).unwrap(),
            "b\n"
        );
        assert!(!source.join("new.txt").exists());
        assert!(!source.join("newdir").exists());
    }

    #[test]
    fn restore_refuses_after_a_later_user_change_and_preserves_it() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("a.txt"), "a\n").unwrap();
        let snapshot_dir = temp.path().join("snapshots/1");
        let manifest = create_snapshot(&source, &snapshot_dir).unwrap();
        // The Coder changes the file.
        std::fs::write(source.join("a.txt"), "coder\n").unwrap();
        let coder_state = current_manifest(&source).unwrap();
        // The user then edits it again.
        std::fs::write(source.join("a.txt"), "user\n").unwrap();
        let error = restore_snapshot(&manifest, &snapshot_dir, &source, &coder_state).unwrap_err();
        assert!(error
            .to_string()
            .contains("refusing to overwrite user changes"));
        assert_eq!(
            std::fs::read_to_string(source.join("a.txt")).unwrap(),
            "user\n"
        );
    }

    #[test]
    fn restore_does_not_delete_files_created_after_the_snapshot() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("a.txt"), "a\n").unwrap();
        let snapshot_dir = temp.path().join("snapshots/1");
        let manifest = create_snapshot(&source, &snapshot_dir).unwrap();
        // The Coder changes a file; the user creates an unrelated file later.
        std::fs::write(source.join("a.txt"), "coder\n").unwrap();
        let coder_state = current_manifest(&source).unwrap();
        std::fs::write(source.join("user.txt"), "user\n").unwrap();
        let error = restore_snapshot(&manifest, &snapshot_dir, &source, &coder_state).unwrap_err();
        assert!(error.to_string().contains("refusing to overwrite"));
        assert_eq!(
            std::fs::read_to_string(source.join("user.txt")).unwrap(),
            "user\n"
        );
    }

    #[test]
    fn path_lock_prevents_two_tasks_from_writing_the_same_directory() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let paths = crate::runtime::RuntimePaths::new(data.clone()).unwrap();
        let store =
            crate::super_agent::TaskStore::new(paths.open_database().unwrap(), data.clone());
        let spec = crate::super_agent::TaskSpec {
            title: "Lock".into(),
            workspace: temp.path().to_path_buf(),
            targets: vec![temp.path().join("scripts").to_string_lossy().into_owned()],
            goal: "Lock a path".into(),
            requirements: vec![],
            acceptance_criteria: vec![],
            constraints: vec![],
            out_of_scope: vec![],
            target_branch: None,
            repository_target_branches: Default::default(),
        };
        let first = store
            .create_task("router", spec.clone(), "create-1")
            .unwrap();
        let second = store.create_task("router", spec, "create-2").unwrap();
        let target = temp.path().join("scripts");
        std::fs::create_dir(&target).unwrap();
        let discovery = super::super::discovery::discover_target(&target, &options(temp.path()));
        let plan = || TargetPlan {
            discovery: discovery.clone(),
            target_branch: None,
        };
        let environment = prepare_environment(
            &first.task_id,
            &data,
            &[plan()],
            Some(&store),
            "isolation:first",
        )
        .unwrap();
        let error = prepare_environment(
            &second.task_id,
            &data,
            &[plan()],
            Some(&store),
            "isolation:second",
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("held by another task"),
            "{error:#}"
        );
        environment.cleanup();
        // After cleanup the lock is free, but the second task still cannot
        // reuse the first task's snapshot directory.
        assert!(store.locks(&first.task_id).unwrap().is_empty());
    }

    #[test]
    fn detection_failures_block_preparation_and_never_initialize_git() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let target = root.join("plain");
        std::fs::create_dir(&target).unwrap();
        let failure = TargetPlan {
            discovery: Discovery::DetectionFailed {
                target: target.clone(),
                error: "permission denied".into(),
            },
            target_branch: None,
        };
        let error =
            prepare_environment("task-004", temp.path(), &[failure], None, "iso").unwrap_err();
        assert!(error.to_string().contains("detection failed"));
        assert!(!target.join(".git").exists());
        let setup = TargetPlan {
            discovery: Discovery::GitSetupRequired {
                target: target.clone(),
                root: None,
                common_dir: None,
                reason: "no commits".into(),
            },
            target_branch: None,
        };
        let error =
            prepare_environment("task-005", temp.path(), &[setup], None, "iso").unwrap_err();
        assert!(error.to_string().contains("setup is required"));
    }

    #[test]
    fn a_duplicate_common_directory_creates_one_worktree() {
        let (temp, root) = repo();
        let discovery = super::super::discovery::discover_target(&root, &options(temp.path()));
        let duplicate = discovery.clone();
        let environment = prepare_environment(
            "task-006",
            temp.path(),
            &[
                TargetPlan {
                    discovery,
                    target_branch: Some("main".into()),
                },
                TargetPlan {
                    discovery: duplicate,
                    target_branch: Some("main".into()),
                },
            ],
            None,
            "iso",
        )
        .unwrap();
        assert_eq!(environment.repositories.len(), 1);
        environment.cleanup();
    }

    #[test]
    fn production_isolation_never_initializes_stashes_or_resets() {
        let source = include_str!("isolation.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        for forbidden in ["\"init\"", "\"stash\"", "\"reset\""] {
            assert!(
                !source.contains(forbidden),
                "production isolation must not run git {forbidden}"
            );
        }
        let worktrees = include_str!("../worktree/session.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap();
        for forbidden in ["\"init\"", "\"stash\"", "\"reset\""] {
            assert!(
                !worktrees.contains(forbidden),
                "production worktree setup must not run git {forbidden}"
            );
        }
    }
}
