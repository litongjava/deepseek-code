//! Internal filesystem apply for a validated [`DiskPlan`].
//!
//! The caller must pass roots and a plan that came from
//! [`super::apply_patch_fs::preflight_patch`]. [`apply_plan`] revalidates the
//! whole plan, stages new bytes in the destination directory, revalidates
//! again, then revalidates each change immediately before its own destructive
//! step before installing it and reporting exactly what happened.
//!
//! # Invariants
//!
//! - Any verification or staging failure happens before a target changes, so
//!   the tree is untouched apart from transient staging files that are removed.
//! - A plan with a stale byte, mode or resolved path (for example a file edited
//!   after preflight, a destination that appeared, or a parent turned into a
//!   symbolic link) is refused; the external state is never overwritten. Each
//!   change is checked again just before it mutates anything, so editing a later
//!   file while an earlier one commits is detected and the earlier change is
//!   rolled back rather than the edit being overwritten.
//! - Unchanged update bytes are reported and not rewritten.
//! - Existing executable bits are carried to updates and move destinations by
//!   staging with the recorded mode. New ordinary files are created with the
//!   platform default permissions, never with an executable bit added. A
//!   failure to apply a promised mode is a commit error, not a warning.
//! - Missing parent directories are created only inside trusted roots, one
//!   component at a time, and recorded.
//! - Install uses an exclusive create (`hard_link` or `create_new`) rather than
//!   an overwriting rename, so a file that appears between the last check and
//!   the install is not clobbered. If the target becomes occupied while an
//!   update install fails, the original is restored with the same
//!   non-overwriting rule; the backup is retained and the path is reported.
//! - The installed object is identified before any post-install inspection or
//!   cleanup can fail (from the staging inode for a hard link, from the open
//!   destination handle for a copy). If inspection or cleanup then fails, the
//!   file is removed again when ownership is provable; otherwise it is left in
//!   place and reported as unrestored, never silently discarded. A replacement
//!   symbolic link or file is never removed.
//! - A staged file is owned until it is installed or the apply returns; an
//!   early error cannot leak it. A staging-cleanup failure after a committed
//!   install is reported as a warning, not as "the target was never installed".
//! - On a controlled failure after a mutation, committed changes are rolled
//!   back in reverse order. Every record stores the canonical path that
//!   preflight already validated against the trusted roots; rollback never
//!   re-resolves it and never follows a final symbolic link. A path is
//!   restored or removed only while it is still the regular file this call
//!   installed (same bytes, Unix mode and device/inode identity); otherwise it
//!   is left in place, its backup is retained and the response lists it as
//!   unrestored.
//!
//! # Honest limits
//!
//! A multi-file apply is **not** crash-atomic: there is no journal and no
//! directory fsync. A process crash between installs can leave some changes
//! applied and others not, with `.dcl-patch-backup-*` files left beside the
//! targets. This module guarantees in-process failure consistency and truthful
//! reporting, not recovery from a crash.
//!
//! A small window still remains between the per-change revalidation and the
//! kernel operation that follows it. This module detects deterministic
//! intervening changes (edits, replacement objects, parent swaps, occupied
//! targets); it does not implement an OS-level concurrency protocol, and two
//! writers racing inside that window can still interleave.
//!
//! # Windows
//!
//! Mode recording and restoration use Unix permission bits. On Windows the
//! recorded mode is `None`, so this module neither verifies nor restores ACL or
//! read-only attributes; replacing a file through rename/hard link may leave
//! the destination with a default ACL. Windows runtime behavior is unverified
//! here and must not be inferred from the Linux tests.
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::apply_patch_fs::{DiskChange, DiskPlan, TrustedRoots};

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Deterministic failure injection for tests. Production callers pass the
/// default (no hooks).
#[derive(Default)]
pub(crate) struct ApplyHooks<'a> {
    /// Runs before change `index` is committed. `Err` aborts with rollback.
    pub before_commit: Option<&'a dyn Fn(usize) -> Result<(), String>>,
    /// Runs after change `index` is committed. `Err` aborts with rollback.
    pub after_commit: Option<&'a dyn Fn(usize) -> Result<(), String>>,
    /// Force the copy fallback instead of hard links (test-only).
    pub force_copy_install: bool,
    /// Observe or fail an install step. The callback may also create a file or
    /// swap a directory to simulate a deterministic intervening change.
    pub install_fault: Option<&'a dyn Fn(usize, InstallFault) -> bool>,
    /// Pretend the staging file cannot be removed after a committed install.
    /// The file is deliberately left in place so the report can name it.
    pub fail_stage_cleanup: Option<&'a dyn Fn(usize) -> bool>,
}

/// Points where the install path can be observed or failed deterministically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InstallFault {
    /// Called immediately before the target is installed. The callback returns
    /// whether to fail, but its side effects run either way.
    BeforeInstallTarget,
    /// Fail the copy fallback after writing but before `sync_all`.
    CopySync,
    /// Fail applying the copied file's permissions.
    CopyPermission,
    /// Fail renaming the original to its backup.
    BackupRename,
    /// Runs immediately after the target is installed and before it is
    /// inspected. The callback may replace the target and returns whether to
    /// force an inspection failure.
    PostInstallInspect,
    /// Fail the cleanup removal of a partially installed target.
    CleanupRemove,
}

/// Returned by [`ApplyHooks::install_fault`] to abort the current step.
fn injected(hooks: &ApplyHooks<'_>, index: usize, fault: InstallFault) -> bool {
    hooks
        .install_fault
        .map(|inject| inject(index, fault))
        .unwrap_or(false)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ApplyReport {
    /// Changes committed and left in place.
    pub applied: Vec<AppliedChange>,
    /// Updates whose new bytes equal the old bytes; nothing was written.
    pub unchanged: Vec<PathBuf>,
    /// Paths committed and then successfully restored during rollback.
    pub rolled_back: Vec<PathBuf>,
    /// Paths left changed because a rollback was impossible or unsafe.
    pub unrestored: Vec<UnrestoredChange>,
    /// Directories created by this call and retained.
    pub created_dirs: Vec<PathBuf>,
    /// Directories created by this call and removed during cleanup.
    pub removed_dirs: Vec<PathBuf>,
    /// The failure that stopped the apply, if any.
    pub error: Option<ApplyError>,
    /// Summary when rollback could not restore every committed path.
    pub rollback_error: Option<String>,
    /// Non-fatal cleanup problems (for example a backup file that could not be
    /// removed after a successful apply).
    pub warnings: Vec<String>,
}

impl ApplyReport {
    /// Every planned change was verified, applied (or unchanged) and no
    /// failure or unrestored path was reported.
    pub(crate) fn succeeded(&self) -> bool {
        self.error.is_none() && self.unrestored.is_empty() && self.rollback_error.is_none()
    }

    /// Paths whose on-disk state no longer matches the caller's intent.
    pub(crate) fn changed_paths(&self) -> Vec<&Path> {
        self.applied
            .iter()
            .map(|change| change.path.as_path())
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AppliedAction {
    Added,
    Updated,
    Deleted,
    Moved,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AppliedChange {
    pub path: PathBuf,
    /// Original patch path, for diagnostics and execution records.
    pub raw_path: String,
    pub action: AppliedAction,
    /// Move destination, if any.
    pub destination: Option<PathBuf>,
    pub destination_raw: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UnrestoredChange {
    pub path: PathBuf,
    pub raw_path: String,
    pub message: String,
    /// Original content kept beside the target when restoration was unsafe.
    pub backup: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum ApplyError {
    #[error("cannot apply '{raw_path}': {message}")]
    Verify {
        raw_path: String,
        path: PathBuf,
        message: String,
    },
    #[error("cannot stage '{path}': {message}")]
    Stage { path: PathBuf, message: String },
    #[error("cannot commit '{path}': {message}")]
    Commit { path: PathBuf, message: String },
    #[error("cannot create directory '{path}': {message}")]
    Directory { path: PathBuf, message: String },
    #[error("apply aborted by a test hook: {0}")]
    Hook(String),
}

/// Apply a freshly validated plan. The returned report always describes the
/// final state; it is not an error type because partial failure must be
/// reported explicitly.
pub(crate) fn apply_plan(
    roots: &TrustedRoots,
    plan: &DiskPlan,
    hooks: ApplyHooks<'_>,
) -> ApplyReport {
    let mut report = ApplyReport::default();

    if let Err(error) = verify_plan(roots, plan) {
        report.error = Some(error);
        return report;
    }

    let mut created_dirs = Vec::new();
    for target in plan.changes.iter().flat_map(targets_of) {
        if let Err(error) = ensure_parent(roots, target, &mut created_dirs) {
            report.error = Some(error);
            report.removed_dirs = cleanup_dirs(&mut created_dirs);
            report.created_dirs = created_dirs;
            return report;
        }
    }

    let mut staged: Vec<Option<StagedFile>> = (0..plan.changes.len()).map(|_| None).collect();
    for (index, change) in plan.changes.iter().enumerate() {
        match stage_change(change) {
            Ok(path) => staged[index] = path,
            Err(error) => {
                cleanup_staged(&mut staged, &mut report.warnings);
                report.error = Some(error);
                report.removed_dirs = cleanup_dirs(&mut created_dirs);
                report.created_dirs = created_dirs;
                return report;
            }
        }
    }

    // Revalidate after staging: the window before commit is the last chance to
    // notice a concurrent edit without having modified a target. Each change is
    // revalidated again immediately before its own destructive step.
    if let Err(error) = verify_plan(roots, plan) {
        cleanup_staged(&mut staged, &mut report.warnings);
        report.error = Some(error);
        report.removed_dirs = cleanup_dirs(&mut created_dirs);
        report.created_dirs = created_dirs;
        return report;
    }

    let mut undo: Vec<(Undo, AppliedChange)> = Vec::new();
    let mut failure: Option<ApplyError> = None;
    for (index, change) in plan.changes.iter().enumerate() {
        if let Some(hook) = hooks.before_commit {
            if let Err(message) = hook(index) {
                failure = Some(ApplyError::Hook(message));
                break;
            }
        }
        // Revalidate this change immediately before its destructive step. This
        // also covers state changed by the hook or by an earlier commit in the
        // same apply (for example a later file edited while file 1 committed).
        if let Err(error) = verify_change(roots, change) {
            failure = Some(error);
            break;
        }
        if is_unchanged(change) {
            report.unchanged.push(change_path(change).to_path_buf());
            continue;
        }
        match commit_one(change, &mut staged[index], &hooks, index) {
            Ok((record, warnings)) => {
                report.warnings.extend(warnings);
                let info = applied_change(change);
                report.applied.push(info.clone());
                undo.push((record, info));
            }
            Err(failure_info) => {
                report.warnings.extend(failure_info.warnings);
                if let Some(recovered) = failure_info.recovered {
                    report.rolled_back.push(recovered);
                }
                if let Some(unrestored) = failure_info.unrestored {
                    report.unrestored.push(unrestored);
                }
                failure = Some(failure_info.error);
                break;
            }
        }
        if let Some(hook) = hooks.after_commit {
            if let Err(message) = hook(index) {
                failure = Some(ApplyError::Hook(message));
                break;
            }
        }
    }

    match failure {
        Some(error) => {
            cleanup_staged(&mut staged, &mut report.warnings);
            let (rolled_back, unrestored, warnings) = rollback(&undo);
            // `report.rolled_back` may already hold a path this failing change
            // installed and removed again; keep it.
            report.rolled_back.extend(rolled_back);
            let rolled = report.rolled_back.clone();
            report
                .applied
                .retain(|change| !rolled.contains(&change.path));
            report.unrestored.extend(unrestored);
            report.warnings.extend(warnings);
            if !report.unrestored.is_empty() {
                report.rollback_error = Some(format!(
                    "{} path(s) could not be restored after the failure; original content is retained where listed",
                    report.unrestored.len()
                ));
            }
            report.removed_dirs = cleanup_dirs(&mut created_dirs);
            report.created_dirs = created_dirs;
            report.error = Some(error);
            report
        }
        None => {
            cleanup_staged(&mut staged, &mut report.warnings);
            for (record, info) in &undo {
                if let Some(backup) = backup_of(record) {
                    if let Err(error) = fs::remove_file(backup) {
                        report.warnings.push(format!(
                            "could not remove backup '{}' for '{}': {error}",
                            backup.display(),
                            info.path.display()
                        ));
                    }
                }
            }
            report.created_dirs = created_dirs;
            report
        }
    }
}

fn targets_of(change: &DiskChange) -> Vec<&Path> {
    match change {
        DiskChange::Add { path, .. }
        | DiskChange::Update { path, .. }
        | DiskChange::Delete { path, .. } => vec![path],
        DiskChange::Move { from, to, .. } => vec![from, to],
    }
}

fn change_path(change: &DiskChange) -> &Path {
    match change {
        DiskChange::Add { path, .. }
        | DiskChange::Update { path, .. }
        | DiskChange::Delete { path, .. } => path,
        DiskChange::Move { from, .. } => from,
    }
}

fn is_unchanged(change: &DiskChange) -> bool {
    matches!(
        change,
        DiskChange::Update {
            old_contents,
            new_contents,
            ..
        } if old_contents == new_contents
    )
}

fn applied_change(change: &DiskChange) -> AppliedChange {
    match change {
        DiskChange::Add { path, raw_path, .. } => AppliedChange {
            path: path.clone(),
            raw_path: raw_path.clone(),
            action: AppliedAction::Added,
            destination: None,
            destination_raw: None,
        },
        DiskChange::Update { path, raw_path, .. } => AppliedChange {
            path: path.clone(),
            raw_path: raw_path.clone(),
            action: AppliedAction::Updated,
            destination: None,
            destination_raw: None,
        },
        DiskChange::Delete { path, raw_path, .. } => AppliedChange {
            path: path.clone(),
            raw_path: raw_path.clone(),
            action: AppliedAction::Deleted,
            destination: None,
            destination_raw: None,
        },
        DiskChange::Move {
            from,
            from_raw,
            to,
            to_raw,
            ..
        } => AppliedChange {
            path: from.clone(),
            raw_path: from_raw.clone(),
            action: AppliedAction::Moved,
            destination: Some(to.clone()),
            destination_raw: Some(to_raw.clone()),
        },
    }
}

/// Revalidate a single change immediately before its destructive step.
fn verify_change(roots: &TrustedRoots, change: &DiskChange) -> Result<(), ApplyError> {
    match change {
        DiskChange::Add { path, raw_path, .. } => verify_new(roots, path, raw_path),
        DiskChange::Update {
            path,
            raw_path,
            old_contents,
            mode,
            ..
        }
        | DiskChange::Delete {
            path,
            raw_path,
            old_contents,
            mode,
        } => verify_existing(roots, path, raw_path, old_contents, *mode),
        DiskChange::Move {
            from,
            from_raw,
            to,
            to_raw,
            old_contents,
            mode,
            ..
        } => {
            verify_existing(roots, from, from_raw, old_contents, *mode)?;
            verify_new(roots, to, to_raw)
        }
    }
}

/// Re-resolve every endpoint from its original patch path and compare with the
/// planned canonical path, mode and bytes. This catches edits, replacements by
/// symlinks, parent swaps and destinations that appeared after preflight.
fn verify_plan(roots: &TrustedRoots, plan: &DiskPlan) -> Result<(), ApplyError> {
    for change in &plan.changes {
        match change {
            DiskChange::Add { path, raw_path, .. } => {
                verify_new(roots, path, raw_path)?;
            }
            DiskChange::Update {
                path,
                raw_path,
                old_contents,
                mode,
                ..
            }
            | DiskChange::Delete {
                path,
                raw_path,
                old_contents,
                mode,
            } => {
                verify_existing(roots, path, raw_path, old_contents, *mode)?;
            }
            DiskChange::Move {
                from,
                from_raw,
                to,
                to_raw,
                old_contents,
                mode,
                ..
            } => {
                verify_existing(roots, from, from_raw, old_contents, *mode)?;
                verify_new(roots, to, to_raw)?;
            }
        }
    }
    Ok(())
}

fn verify_new(roots: &TrustedRoots, path: &Path, raw: &str) -> Result<(), ApplyError> {
    let resolved = roots
        .revalidate_new(raw)
        .map_err(|error| ApplyError::Verify {
            raw_path: raw.to_string(),
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
    if resolved != path {
        return Err(ApplyError::Verify {
            raw_path: raw.to_string(),
            path: path.to_path_buf(),
            message: format!(
                "the path now resolves to '{}'; it changed after preflight",
                resolved.display()
            ),
        });
    }
    Ok(())
}

fn verify_existing(
    roots: &TrustedRoots,
    path: &Path,
    raw: &str,
    old_contents: &[u8],
    mode: Option<u32>,
) -> Result<(), ApplyError> {
    let (resolved, current_mode) =
        roots
            .revalidate_existing(raw)
            .map_err(|error| ApplyError::Verify {
                raw_path: raw.to_string(),
                path: path.to_path_buf(),
                message: error.to_string(),
            })?;
    if resolved != path {
        return Err(ApplyError::Verify {
            raw_path: raw.to_string(),
            path: path.to_path_buf(),
            message: format!(
                "the path now resolves to '{}'; it changed after preflight",
                resolved.display()
            ),
        });
    }
    if current_mode != mode {
        return Err(ApplyError::Verify {
            raw_path: raw.to_string(),
            path: path.to_path_buf(),
            message: format!(
                "permissions changed since preflight (expected {mode:?}, found {current_mode:?})"
            ),
        });
    }
    let current = fs::read(path).map_err(|error| ApplyError::Verify {
        raw_path: raw.to_string(),
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    if current != old_contents {
        return Err(ApplyError::Verify {
            raw_path: raw.to_string(),
            path: path.to_path_buf(),
            message: "content changed since preflight; refusing to overwrite the concurrent edit"
                .into(),
        });
    }
    Ok(())
}

/// Create the missing ancestors of `target`, one component at a time, and record
/// exactly which directories this call created.
fn ensure_parent(
    roots: &TrustedRoots,
    target: &Path,
    created: &mut Vec<PathBuf>,
) -> Result<(), ApplyError> {
    let Some(mut current) = target.parent().map(Path::to_path_buf) else {
        return Err(ApplyError::Directory {
            path: target.to_path_buf(),
            message: "the target has no parent directory".into(),
        });
    };
    let mut missing = Vec::new();
    loop {
        match fs::symlink_metadata(&current) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(ApplyError::Directory {
                        path: current,
                        message: "a parent directory is a symbolic link; refusing to follow it"
                            .into(),
                    });
                }
                if !metadata.is_dir() {
                    return Err(ApplyError::Directory {
                        path: current,
                        message: "a parent component is not a directory".into(),
                    });
                }
                let canonical =
                    fs::canonicalize(&current).map_err(|error| ApplyError::Directory {
                        path: current.clone(),
                        message: error.to_string(),
                    })?;
                if canonical != current {
                    return Err(ApplyError::Directory {
                        path: current,
                        message: "a parent directory changed identity since preflight".into(),
                    });
                }
                if !roots.contains(&canonical) {
                    return Err(ApplyError::Directory {
                        path: canonical,
                        message: "a parent directory is outside the trusted roots".into(),
                    });
                }
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                missing.push(current.clone());
                let Some(parent) = current.parent().map(Path::to_path_buf) else {
                    return Err(ApplyError::Directory {
                        path: current,
                        message: "no existing parent directory inside a trusted root".into(),
                    });
                };
                current = parent;
            }
            Err(error) if error.kind() == io::ErrorKind::NotADirectory => {
                return Err(ApplyError::Directory {
                    path: current,
                    message: "a parent component is not a directory".into(),
                });
            }
            Err(error) => {
                return Err(ApplyError::Directory {
                    path: current,
                    message: error.to_string(),
                });
            }
        }
    }

    for directory in missing.iter().rev() {
        if !roots.contains(directory) {
            return Err(ApplyError::Directory {
                path: directory.clone(),
                message: "refusing to create a directory outside the trusted roots".into(),
            });
        }
        match fs::create_dir(directory) {
            Ok(()) => {
                created.push(directory.clone());
                let canonical =
                    fs::canonicalize(directory).map_err(|error| ApplyError::Directory {
                        path: directory.clone(),
                        message: error.to_string(),
                    })?;
                if canonical != *directory {
                    return Err(ApplyError::Directory {
                        path: directory.clone(),
                        message: "the created directory changed identity".into(),
                    });
                }
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                if !fs::symlink_metadata(directory)
                    .map(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
                    .unwrap_or(false)
                {
                    return Err(ApplyError::Directory {
                        path: directory.clone(),
                        message: "a non-directory appeared where a parent was expected".into(),
                    });
                }
            }
            Err(error) => {
                return Err(ApplyError::Directory {
                    path: directory.clone(),
                    message: error.to_string(),
                });
            }
        }
    }
    Ok(())
}

/// Owns a staged file for the lifetime of the apply. If the apply stops before
/// installing it, `Drop` removes it; explicit [`StagedFile::cleanup`] reports a
/// failure as a warning instead of leaking silently.
struct StagedFile {
    path: PathBuf,
    armed: bool,
}

impl StagedFile {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn cleanup(&mut self, warnings: &mut Vec<String>, force_failure: bool) {
        if !self.armed {
            return;
        }
        self.armed = false;
        if force_failure {
            warnings.push(format!(
                "could not remove staging file '{}' (simulated failure); the install itself succeeded",
                self.path.display()
            ));
            return;
        }
        match fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => warnings.push(format!(
                "could not remove staging file '{}' after a committed change: {error}",
                self.path.display()
            )),
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for StagedFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Write the new bytes to a unique file in the target directory. Returns `None`
/// for an unchanged update. The staged file is never a target of the patch.
fn stage_change(change: &DiskChange) -> Result<Option<StagedFile>, ApplyError> {
    match change {
        DiskChange::Delete { .. } => Ok(None),
        DiskChange::Add { path, contents, .. } => {
            stage_file(path, contents, None).map(|path| Some(StagedFile::new(path)))
        }
        DiskChange::Update { .. } if is_unchanged(change) => Ok(None),
        DiskChange::Update {
            path,
            new_contents,
            mode,
            ..
        } => stage_file(path, new_contents, *mode).map(|path| Some(StagedFile::new(path))),
        DiskChange::Move {
            to,
            new_contents,
            mode,
            ..
        } => stage_file(to, new_contents, *mode).map(|path| Some(StagedFile::new(path))),
    }
}

fn stage_file(target: &Path, contents: &[u8], mode: Option<u32>) -> Result<PathBuf, ApplyError> {
    for _ in 0..32 {
        let staged = temp_path(target, "stage");
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staged)
        {
            Ok(mut file) => {
                let written = file.write_all(contents).and_then(|()| file.sync_all());
                drop(file);
                if let Err(error) = written {
                    let _ = fs::remove_file(&staged);
                    return Err(stage_error(target, error));
                }
                if let Err(error) = apply_mode(&staged, mode) {
                    let _ = fs::remove_file(&staged);
                    return Err(stage_error(target, error));
                }
                return Ok(staged);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(stage_error(target, error)),
        }
    }
    Err(stage_error(
        target,
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not reserve a unique staging name",
        ),
    ))
}

fn stage_error(path: &Path, error: io::Error) -> ApplyError {
    ApplyError::Stage {
        path: path.to_path_buf(),
        message: error.to_string(),
    }
}

#[cfg(unix)]
fn apply_mode(path: &Path, mode: Option<u32>) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    match mode {
        Some(bits) => fs::set_permissions(path, fs::Permissions::from_mode(bits)),
        None => Ok(()),
    }
}

#[cfg(not(unix))]
fn apply_mode(_path: &Path, _mode: Option<u32>) -> io::Result<()> {
    // Windows ACL / read-only handling is a separate contract; see module docs.
    Ok(())
}

#[cfg(unix)]
fn permission_bits(metadata: &fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(metadata.permissions().mode() & 0o777)
}

#[cfg(not(unix))]
fn permission_bits(_metadata: &fs::Metadata) -> Option<u32> {
    None
}

fn temp_path(target: &Path, kind: &str) -> PathBuf {
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!(
        ".{name}.dcl-patch-{kind}-{}-{counter}",
        std::process::id()
    ))
}

fn reserve_backup(target: &Path) -> io::Result<PathBuf> {
    for _ in 0..32 {
        let candidate = temp_path(target, "backup");
        if fs::symlink_metadata(&candidate).is_err() {
            return Ok(candidate);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not reserve a backup name",
    ))
}

enum Undo {
    Added {
        path: PathBuf,
        raw: String,
        contents: Vec<u8>,
        mode: Option<u32>,
        identity: Option<FileIdentity>,
    },
    Updated {
        path: PathBuf,
        raw: String,
        backup: PathBuf,
        new_contents: Vec<u8>,
        new_mode: Option<u32>,
        identity: Option<FileIdentity>,
    },
    Deleted {
        path: PathBuf,
        raw: String,
        backup: PathBuf,
    },
    Moved {
        from: PathBuf,
        from_raw: String,
        to: PathBuf,
        to_raw: String,
        backup: PathBuf,
        new_contents: Vec<u8>,
        new_mode: Option<u32>,
        identity: Option<FileIdentity>,
    },
}

fn backup_of(record: &Undo) -> Option<&Path> {
    match record {
        Undo::Added { .. } => None,
        Undo::Updated { backup, .. }
        | Undo::Deleted { backup, .. }
        | Undo::Moved { backup, .. } => Some(backup),
    }
}

struct CommitFailure {
    error: ApplyError,
    unrestored: Option<UnrestoredChange>,
    warnings: Vec<String>,
    /// A path this change installed and then removed again before returning, so
    /// the final report can list it as rolled back rather than as untouched.
    recovered: Option<PathBuf>,
}

impl CommitFailure {
    fn new(error: ApplyError) -> Self {
        Self {
            error,
            unrestored: None,
            warnings: Vec::new(),
            recovered: None,
        }
    }
}

/// A target this call created with an exclusive install. `identity` is known
/// for hard links (from the staging inode) and, normally, for copied files
/// (from the open destination handle); it is `None` only if the handle
/// metadata read failed, in which case cleanup cannot prove ownership.
struct InstalledTarget {
    path: PathBuf,
    mode: Option<u32>,
    identity: Option<FileIdentity>,
}

struct InstallOutcome {
    target: InstalledTarget,
    warnings: Vec<String>,
}

/// Result of a failed install. `installed` is `Some` once the exclusive install
/// created the target, so the caller can remove it safely or report it.
struct InstallError {
    error: io::Error,
    installed: Option<InstalledTarget>,
    warnings: Vec<String>,
}

impl InstallError {
    fn plain(error: io::Error) -> Self {
        Self {
            error,
            installed: None,
            warnings: Vec::new(),
        }
    }
}

/// Identifies the filesystem object this call installed, so a rollback cannot
/// mistake a replacement file (even one with identical bytes) for its own.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    dev: u64,
    ino: u64,
}

#[cfg(unix)]
fn identity_of(metadata: &fs::Metadata) -> FileIdentity {
    use std::os::unix::fs::MetadataExt;
    FileIdentity {
        dev: metadata.dev(),
        ino: metadata.ino(),
    }
}

#[cfg(not(unix))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileIdentity;

#[cfg(not(unix))]
fn identity_of(_metadata: &fs::Metadata) -> FileIdentity {
    FileIdentity
}

fn commit_one(
    change: &DiskChange,
    staged: &mut Option<StagedFile>,
    hooks: &ApplyHooks<'_>,
    index: usize,
) -> Result<(Undo, Vec<String>), CommitFailure> {
    match change {
        DiskChange::Add {
            path,
            raw_path,
            contents,
        } => {
            let Some(staged) = staged.as_mut() else {
                return Err(internal_failure(path));
            };
            match install_new(staged, path, None, hooks, index) {
                Ok(outcome) => Ok((
                    Undo::Added {
                        path: path.clone(),
                        raw: raw_path.clone(),
                        contents: contents.clone(),
                        mode: outcome.target.mode,
                        identity: outcome.target.identity,
                    },
                    outcome.warnings,
                )),
                Err(failure) => Err(recover_add_failure(path, raw_path, failure, hooks, index)),
            }
        }
        DiskChange::Update {
            path,
            raw_path,
            new_contents,
            mode,
            ..
        } => {
            let Some(staged) = staged.as_mut() else {
                return Err(internal_failure(path));
            };
            let backup = reserve_backup(path)
                .map_err(|error| CommitFailure::new(commit_error(path, error)))?;
            move_to_backup(path, &backup, hooks, index)
                .map_err(|error| CommitFailure::new(commit_error(path, error)))?;
            match install_new(staged, path, *mode, hooks, index) {
                Ok(outcome) => Ok((
                    Undo::Updated {
                        path: path.clone(),
                        raw: raw_path.clone(),
                        backup,
                        new_contents: new_contents.clone(),
                        new_mode: outcome.target.mode,
                        identity: outcome.target.identity,
                    },
                    outcome.warnings,
                )),
                Err(failure) => Err(recover_replaced_failure(
                    path,
                    path,
                    raw_path,
                    backup,
                    failure,
                    hooks,
                    index,
                    "installing the new content",
                )),
            }
        }
        DiskChange::Delete { path, raw_path, .. } => {
            let backup = reserve_backup(path)
                .map_err(|error| CommitFailure::new(commit_error(path, error)))?;
            move_to_backup(path, &backup, hooks, index)
                .map_err(|error| CommitFailure::new(commit_error(path, error)))?;
            Ok((
                Undo::Deleted {
                    path: path.clone(),
                    raw: raw_path.clone(),
                    backup,
                },
                Vec::new(),
            ))
        }
        DiskChange::Move {
            from,
            from_raw,
            to,
            to_raw,
            new_contents,
            mode,
            ..
        } => {
            let Some(staged) = staged.as_mut() else {
                return Err(internal_failure(to));
            };
            let backup = reserve_backup(from)
                .map_err(|error| CommitFailure::new(commit_error(from, error)))?;
            move_to_backup(from, &backup, hooks, index)
                .map_err(|error| CommitFailure::new(commit_error(from, error)))?;
            match install_new(staged, to, *mode, hooks, index) {
                Ok(outcome) => Ok((
                    Undo::Moved {
                        from: from.clone(),
                        from_raw: from_raw.clone(),
                        to: to.clone(),
                        to_raw: to_raw.clone(),
                        backup,
                        new_contents: new_contents.clone(),
                        new_mode: outcome.target.mode,
                        identity: outcome.target.identity,
                    },
                    outcome.warnings,
                )),
                Err(failure) => Err(recover_replaced_failure(
                    to,
                    from,
                    from_raw,
                    backup,
                    failure,
                    hooks,
                    index,
                    "installing the destination",
                )),
            }
        }
    }
}

/// Turn a partially installed Add into a failure that either removes the file
/// again or reports it as unrestored.
fn recover_add_failure(
    path: &Path,
    raw_path: &str,
    failure: InstallError,
    hooks: &ApplyHooks<'_>,
    index: usize,
) -> CommitFailure {
    let InstallError {
        error,
        installed,
        mut warnings,
    } = failure;
    let message = error.to_string();
    let Some(target) = installed else {
        return CommitFailure {
            error: commit_error(path, error),
            unrestored: None,
            warnings,
            recovered: None,
        };
    };
    match remove_installed(&target, hooks, index) {
        Ok(()) => {
            warnings.push(format!(
                "the file was installed but could not be verified ({message}); it was removed again"
            ));
            CommitFailure {
                error: commit_error(path, error),
                unrestored: None,
                warnings,
                recovered: Some(path.to_path_buf()),
            }
        }
        Err(cleanup) => {
            warnings.push(format!(
                "the installed file could not be cleaned up: {cleanup}"
            ));
            CommitFailure {
                error: commit_error(path, error),
                unrestored: Some(UnrestoredChange {
                    path: path.to_path_buf(),
                    raw_path: raw_path.to_string(),
                    message: format!(
                        "the file was installed but could not be verified ({message}) and could not be removed ({cleanup}); it remains at this path"
                    ),
                    backup: None,
                }),
                warnings,
                recovered: None,
            }
        }
    }
}

/// Turn a failed Update/Move install into a failure that removes the partial
/// install when provable, restores the backup, and reports an occupied target
/// with the retained backup.
#[allow(clippy::too_many_arguments)]
fn recover_replaced_failure(
    error_path: &Path,
    record_path: &Path,
    record_raw: &str,
    backup: PathBuf,
    failure: InstallError,
    hooks: &ApplyHooks<'_>,
    index: usize,
    label: &str,
) -> CommitFailure {
    let InstallError {
        error,
        installed,
        mut warnings,
    } = failure;
    let mut cleanup_problem = None;
    if let Some(target) = installed.as_ref() {
        if let Err(reason) = remove_installed(target, hooks, index) {
            cleanup_problem = Some(reason);
        }
    }
    match restore_backup(&backup, record_path, &mut warnings) {
        Ok(()) => {
            if let Some(problem) = cleanup_problem {
                warnings.push(format!(
                    "the original was restored, but the partially installed file was not cleaned up: {problem}"
                ));
            }
            CommitFailure {
                error: commit_error(error_path, error),
                unrestored: None,
                warnings,
                recovered: Some(record_path.to_path_buf()),
            }
        }
        Err(restore) => {
            let mut message = format!(
                "{label} failed ({error}); the original was not restored because that would overwrite another file: {restore}"
            );
            if let Some(problem) = cleanup_problem {
                message.push_str(&format!(
                    "; the partially installed file could not be removed: {problem}"
                ));
            }
            CommitFailure {
                error: commit_error(error_path, error),
                unrestored: Some(UnrestoredChange {
                    path: record_path.to_path_buf(),
                    raw_path: record_raw.to_string(),
                    message,
                    backup: Some(backup),
                }),
                warnings,
                recovered: None,
            }
        }
    }
}

/// Remove a path only while it is provably the object this call installed.
/// Refuses (without touching anything) when the path is a symbolic link, is no
/// longer a regular file, changed identity, or changed permissions. A removal
/// failure is returned rather than swallowed.
fn remove_installed(
    target: &InstalledTarget,
    hooks: &ApplyHooks<'_>,
    index: usize,
) -> Result<(), String> {
    let metadata = match fs::symlink_metadata(&target.path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "could not inspect '{}': {error}",
                target.path.display()
            ))
        }
    };
    if metadata.file_type().is_symlink() {
        return Err(format!(
            "'{}' is now a symbolic link; it was left in place",
            target.path.display()
        ));
    }
    if !metadata.is_file() {
        return Err(format!(
            "'{}' is no longer a regular file; it was left in place",
            target.path.display()
        ));
    }
    if let Some(expected) = target.identity {
        if identity_of(&metadata) != expected {
            return Err(format!(
                "'{}' was replaced by another object; it was left in place",
                target.path.display()
            ));
        }
    }
    if let (Some(expected), Some(actual)) = (target.mode, permission_bits(&metadata)) {
        if expected != actual {
            return Err(format!(
                "'{}' permissions were changed by another process; it was left in place",
                target.path.display()
            ));
        }
    }
    if injected(hooks, index, InstallFault::CleanupRemove) {
        return Err(format!(
            "removing '{}' failed: simulated cleanup failure",
            target.path.display()
        ));
    }
    fs::remove_file(&target.path)
        .map_err(|error| format!("removing '{}' failed: {error}", target.path.display()))
}

fn internal_failure(path: &Path) -> CommitFailure {
    CommitFailure::new(ApplyError::Commit {
        path: path.to_path_buf(),
        message: "internal: no staged file".into(),
    })
}

fn move_to_backup(
    source: &Path,
    backup: &Path,
    hooks: &ApplyHooks<'_>,
    index: usize,
) -> io::Result<()> {
    if injected(hooks, index, InstallFault::BackupRename) {
        return Err(io::Error::new(
            io::ErrorKind::Other,
            "simulated backup rename failure",
        ));
    }
    fs::rename(source, backup)
}

/// Install a staged file without clobbering anything that appeared at `target`,
/// then clean up the staging name.
///
/// The installed object's identity is captured before any fallible step that
/// follows the exclusive install: from the staging inode for a hard link and
/// from the open destination handle for a copy. If post-install inspection or
/// staging cleanup fails, the partial install travels back to the caller in
/// [`InstallError::installed`] so it can be removed or reported instead of
/// being lost.
fn install_new(
    staged: &mut StagedFile,
    target: &Path,
    mode: Option<u32>,
    hooks: &ApplyHooks<'_>,
    index: usize,
) -> Result<InstallOutcome, InstallError> {
    if injected(hooks, index, InstallFault::BeforeInstallTarget) {
        return Err(InstallError::plain(io::Error::new(
            io::ErrorKind::Other,
            "simulated install failure before the target was installed",
        )));
    }

    let installed = if hooks.force_copy_install {
        match copy_install(staged.path(), target, mode, hooks, index) {
            Ok(target_info) => target_info,
            Err(mut failure) => {
                cleanup_staged_after_install_error(staged, hooks, index, &mut failure.warnings);
                return Err(failure);
            }
        }
    } else {
        // Stat the staging file (owned by this call) before linking so the
        // installed object's identity is known even if later inspection fails.
        let staging_metadata = fs::symlink_metadata(staged.path()).map_err(InstallError::plain)?;
        let known = InstalledTarget {
            path: target.to_path_buf(),
            mode: permission_bits(&staging_metadata),
            identity: Some(identity_of(&staging_metadata)),
        };
        match fs::hard_link(staged.path(), target) {
            Ok(()) => known,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                return Err(InstallError::plain(error));
            }
            Err(_) => {
                // Hard links may be unavailable on some filesystems; fall back
                // to an exclusive create and copy.
                match copy_install(staged.path(), target, mode, hooks, index) {
                    Ok(target_info) => target_info,
                    Err(mut failure) => {
                        cleanup_staged_after_install_error(
                            staged,
                            hooks,
                            index,
                            &mut failure.warnings,
                        );
                        return Err(failure);
                    }
                }
            }
        }
    };

    let inspected = if injected(hooks, index, InstallFault::PostInstallInspect) {
        Err(io::Error::new(
            io::ErrorKind::Other,
            "simulated post-install inspection failure",
        ))
    } else {
        inspect_installed(target, installed.identity)
    };
    let inspected = match inspected {
        Ok(metadata) => metadata,
        Err(error) => {
            let mut warnings = Vec::new();
            cleanup_staged_after_install_error(staged, hooks, index, &mut warnings);
            return Err(InstallError {
                error,
                installed: Some(installed),
                warnings,
            });
        }
    };

    let final_target = InstalledTarget {
        path: installed.path,
        mode: permission_bits(&inspected),
        identity: Some(identity_of(&inspected)),
    };
    let mut warnings = Vec::new();
    cleanup_staged_after_install_error(staged, hooks, index, &mut warnings);
    Ok(InstallOutcome {
        target: final_target,
        warnings,
    })
}

/// Inspect a freshly installed target. A symbolic link, a non-regular file or
/// an object whose identity is not the one this call installed is rejected; the
/// caller then must not remove it.
fn inspect_installed(target: &Path, expected: Option<FileIdentity>) -> io::Result<fs::Metadata> {
    let metadata = fs::symlink_metadata(target)?;
    if metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the installed target was replaced by a symbolic link",
        ));
    }
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "the installed target is not a regular file",
        ));
    }
    if let Some(expected) = expected {
        if identity_of(&metadata) != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the installed target was replaced by another object",
            ));
        }
    }
    Ok(metadata)
}

fn cleanup_staged_after_install_error(
    staged: &mut StagedFile,
    hooks: &ApplyHooks<'_>,
    index: usize,
    warnings: &mut Vec<String>,
) {
    let force = hooks
        .fail_stage_cleanup
        .map(|predicate| predicate(index))
        .unwrap_or(false);
    staged.cleanup(warnings, force);
}

/// Copy the staged bytes into an exclusively created target. Once `create_new`
/// succeeds the target belongs to this call; a later failure returns it in the
/// error so the caller can clean it up or report it.
fn copy_install(
    source: &Path,
    target: &Path,
    mode: Option<u32>,
    hooks: &ApplyHooks<'_>,
    index: usize,
) -> Result<InstalledTarget, InstallError> {
    let mut source_file = File::open(source).map_err(InstallError::plain)?;
    let mut destination = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)
        .map_err(InstallError::plain)?;
    // The exclusive create succeeded: capture the identity from the open
    // handle before any later step can fail.
    let handle_metadata = destination.metadata().ok();
    let known = InstalledTarget {
        path: target.to_path_buf(),
        mode: handle_metadata.as_ref().and_then(permission_bits),
        identity: handle_metadata.as_ref().map(identity_of),
    };
    let copied = (|| -> io::Result<()> {
        io::copy(&mut source_file, &mut destination)?;
        if injected(hooks, index, InstallFault::CopySync) {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "simulated copy sync failure",
            ));
        }
        destination.sync_all()
    })();
    drop(destination);
    let copied = copied.and_then(|()| {
        // The staged file already carries the requested mode. Copying it keeps
        // the target consistent; when a mode was promised, a failure here must
        // not be swallowed.
        let permissions = fs::metadata(source)?.permissions();
        if injected(hooks, index, InstallFault::CopyPermission) {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "simulated permission failure",
            ));
        }
        match fs::set_permissions(target, permissions) {
            Ok(()) => Ok(()),
            Err(error) if mode.is_some() => Err(error),
            Err(_) => Ok(()),
        }
    });
    match copied {
        Ok(()) => Ok(known),
        Err(error) => Err(InstallError {
            error,
            installed: Some(known),
            warnings: Vec::new(),
        }),
    }
}

/// Restore a backup without overwriting anything that appeared at the target.
/// `hard_link` (or an exclusive copy fallback) fails when the path is occupied,
/// so an external file created while the install was failing is preserved.
fn restore_backup(backup: &Path, target: &Path, warnings: &mut Vec<String>) -> io::Result<()> {
    match fs::hard_link(backup, target) {
        Ok(()) => {
            if let Err(error) = fs::remove_file(backup) {
                warnings.push(format!(
                    "restored '{}' but could not remove backup '{}': {error}",
                    target.display(),
                    backup.display()
                ));
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Err(error),
        Err(_) => {
            let mut source = File::open(backup)?;
            let mut destination = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(target)?;
            let copied = (|| -> io::Result<()> {
                io::copy(&mut source, &mut destination)?;
                destination.sync_all()
            })();
            drop(destination);
            let copied = copied.and_then(|()| {
                let permissions = fs::metadata(backup)?.permissions();
                fs::set_permissions(target, permissions)
            });
            match copied {
                Ok(()) => {
                    if let Err(error) = fs::remove_file(backup) {
                        warnings.push(format!(
                            "restored '{}' but could not remove backup '{}': {error}",
                            target.display(),
                            backup.display()
                        ));
                    }
                    Ok(())
                }
                Err(error) => {
                    let _ = fs::remove_file(target);
                    Err(error)
                }
            }
        }
    }
}

fn commit_error(path: &Path, error: io::Error) -> ApplyError {
    ApplyError::Commit {
        path: path.to_path_buf(),
        message: error.to_string(),
    }
}

fn rollback(
    records: &[(Undo, AppliedChange)],
) -> (Vec<PathBuf>, Vec<UnrestoredChange>, Vec<String>) {
    let mut rolled_back = Vec::new();
    let mut unrestored_changes = Vec::new();
    let mut warnings = Vec::new();
    for (record, _) in records.iter().rev() {
        match record {
            Undo::Added {
                path,
                raw,
                contents,
                mode,
                identity,
            } => rollback_added(
                path,
                raw,
                contents,
                *mode,
                *identity,
                &mut rolled_back,
                &mut unrestored_changes,
            ),
            Undo::Updated {
                path,
                raw,
                backup,
                new_contents,
                new_mode,
                identity,
            } => rollback_updated(
                path,
                raw,
                backup,
                new_contents,
                *new_mode,
                *identity,
                &mut rolled_back,
                &mut unrestored_changes,
                &mut warnings,
            ),
            Undo::Deleted { path, raw, backup } => rollback_deleted(
                path,
                raw,
                backup,
                &mut rolled_back,
                &mut unrestored_changes,
                &mut warnings,
            ),
            Undo::Moved {
                from,
                from_raw,
                to,
                to_raw,
                backup,
                new_contents,
                new_mode,
                identity,
            } => rollback_moved(
                from,
                from_raw,
                to,
                to_raw,
                backup,
                new_contents,
                *new_mode,
                *identity,
                &mut rolled_back,
                &mut unrestored_changes,
                &mut warnings,
            ),
        }
    }
    if !unrestored_changes.is_empty() {
        warnings.push(format!(
            "{} path(s) were not restored; their original content is retained where listed",
            unrestored_changes.len()
        ));
    }
    (rolled_back, unrestored_changes, warnings)
}

fn unrestored_change(
    path: &Path,
    raw: &str,
    message: String,
    backup: Option<&Path>,
) -> UnrestoredChange {
    UnrestoredChange {
        path: path.to_path_buf(),
        raw_path: raw.to_string(),
        message,
        backup: backup.map(Path::to_path_buf),
    }
}

/// Why the current object is not the file this call installed, if it differs.
/// Checks object kind, bytes, Unix mode and (on Unix) device/inode identity.
fn installed_mismatch(
    metadata: &fs::Metadata,
    current: &[u8],
    contents: &[u8],
    mode: Option<u32>,
    identity: Option<FileIdentity>,
) -> Option<String> {
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Some("the path is no longer a regular file".into());
    }
    if current != contents {
        return Some("the content was changed by another process".into());
    }
    if let Some(expected) = mode {
        if permission_bits(metadata) != Some(expected) {
            return Some("the permissions were changed by another process".into());
        }
    }
    if let Some(expected) = identity {
        if identity_of(metadata) != expected {
            return Some("the path was replaced by another object".into());
        }
    }
    None
}

fn rollback_added(
    path: &Path,
    raw: &str,
    contents: &[u8],
    mode: Option<u32>,
    identity: Option<FileIdentity>,
    rolled_back: &mut Vec<PathBuf>,
    unrestored: &mut Vec<UnrestoredChange>,
) {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            rolled_back.push(path.to_path_buf());
        }
        Err(error) => unrestored.push(unrestored_change(
            path,
            raw,
            format!("inspecting the added file failed: {error}"),
            None,
        )),
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                unrestored.push(unrestored_change(
                    path,
                    raw,
                    "the added file was replaced by a symbolic link; left in place".into(),
                    None,
                ));
            } else if !metadata.is_file() {
                unrestored.push(unrestored_change(
                    path,
                    raw,
                    "the added path is no longer a regular file; left in place".into(),
                    None,
                ));
            } else {
                match fs::read(path) {
                    Ok(current) => {
                        match installed_mismatch(&metadata, &current, contents, mode, identity) {
                            Some(reason) => unrestored.push(unrestored_change(
                                path,
                                raw,
                                format!(
                                    "the added file was changed by another process ({reason}); left in place"
                                ),
                                None,
                            )),
                            None => match fs::remove_file(path) {
                                Ok(()) => rolled_back.push(path.to_path_buf()),
                                Err(error) => unrestored.push(unrestored_change(
                                    path,
                                    raw,
                                    format!("removing the added file failed: {error}"),
                                    None,
                                )),
                            },
                        }
                    }
                    Err(error) => unrestored.push(unrestored_change(
                        path,
                        raw,
                        format!("reading the added file failed: {error}"),
                        None,
                    )),
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn rollback_updated(
    path: &Path,
    raw: &str,
    backup: &Path,
    new_contents: &[u8],
    new_mode: Option<u32>,
    identity: Option<FileIdentity>,
    rolled_back: &mut Vec<PathBuf>,
    unrestored: &mut Vec<UnrestoredChange>,
    warnings: &mut Vec<String>,
) {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            unrestored.push(unrestored_change(
                path,
                raw,
                "the updated path was replaced by a symbolic link; the original is retained".into(),
                Some(backup),
            ));
        }
        Ok(metadata) if !metadata.is_file() => {
            unrestored.push(unrestored_change(
                path,
                raw,
                "the updated path is no longer a regular file; the original is retained".into(),
                Some(backup),
            ));
        }
        Ok(metadata) => match fs::read(path) {
            Ok(current) => match installed_mismatch(&metadata, &current, new_contents, new_mode, identity)
            {
                Some(reason) => unrestored.push(unrestored_change(
                    path,
                    raw,
                    format!(
                        "the updated file was changed by another process ({reason}); the original is retained"
                    ),
                    Some(backup),
                )),
                None => {
                    if let Err(error) = fs::remove_file(path) {
                        unrestored.push(unrestored_change(
                            path,
                            raw,
                            format!("removing the updated file failed: {error}"),
                            Some(backup),
                        ));
                        return;
                    }
                    match restore_backup(backup, path, warnings) {
                        Ok(()) => rolled_back.push(path.to_path_buf()),
                        Err(error) => unrestored.push(unrestored_change(
                            path,
                            raw,
                            format!("restoring the original failed: {error}"),
                            Some(backup),
                        )),
                    }
                }
            },
            Err(error) => unrestored.push(unrestored_change(
                path,
                raw,
                format!("reading the updated file failed: {error}"),
                Some(backup),
            )),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match restore_backup(backup, path, warnings) {
                Ok(()) => rolled_back.push(path.to_path_buf()),
                Err(error) => unrestored.push(unrestored_change(
                    path,
                    raw,
                    format!("restoring the missing file failed: {error}"),
                    Some(backup),
                )),
            }
        }
        Err(error) => unrestored.push(unrestored_change(
            path,
            raw,
            format!("inspecting the updated path failed: {error}"),
            Some(backup),
        )),
    }
}

fn rollback_deleted(
    path: &Path,
    raw: &str,
    backup: &Path,
    rolled_back: &mut Vec<PathBuf>,
    unrestored: &mut Vec<UnrestoredChange>,
    warnings: &mut Vec<String>,
) {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match restore_backup(backup, path, warnings) {
                Ok(()) => rolled_back.push(path.to_path_buf()),
                Err(error) => unrestored.push(unrestored_change(
                    path,
                    raw,
                    format!("restoring the deleted file failed: {error}"),
                    Some(backup),
                )),
            }
        }
        Ok(_) => unrestored.push(unrestored_change(
            path,
            raw,
            "a new file appeared at the deleted path; the original is retained and was not restored over it"
                .into(),
            Some(backup),
        )),
        Err(error) => unrestored.push(unrestored_change(
            path,
            raw,
            format!("inspecting the deleted path failed: {error}"),
            Some(backup),
        )),
    }
}

#[allow(clippy::too_many_arguments)]
fn rollback_moved(
    from: &Path,
    from_raw: &str,
    to: &Path,
    to_raw: &str,
    backup: &Path,
    new_contents: &[u8],
    new_mode: Option<u32>,
    identity: Option<FileIdentity>,
    rolled_back: &mut Vec<PathBuf>,
    unrestored: &mut Vec<UnrestoredChange>,
    warnings: &mut Vec<String>,
) {
    let mut destination_ok = true;
    match fs::symlink_metadata(to) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            destination_ok = false;
            unrestored.push(unrestored_change(
                to,
                to_raw,
                "the move destination is no longer a regular file; left in place".into(),
                None,
            ));
        }
        Ok(metadata) => match fs::read(to) {
            Ok(current) => {
                match installed_mismatch(&metadata, &current, new_contents, new_mode, identity) {
                    Some(reason) => {
                        destination_ok = false;
                        unrestored.push(unrestored_change(
                        to,
                        to_raw,
                        format!(
                            "the move destination was changed by another process ({reason}); left in place"
                        ),
                        None,
                    ));
                    }
                    None => {
                        if let Err(error) = fs::remove_file(to) {
                            destination_ok = false;
                            unrestored.push(unrestored_change(
                                to,
                                to_raw,
                                format!("removing the moved destination failed: {error}"),
                                None,
                            ));
                        }
                    }
                }
            }
            Err(error) => {
                destination_ok = false;
                unrestored.push(unrestored_change(
                    to,
                    to_raw,
                    format!("reading the move destination failed: {error}"),
                    None,
                ));
            }
        },
        Err(error) => {
            destination_ok = false;
            unrestored.push(unrestored_change(
                to,
                to_raw,
                format!("inspecting the move destination failed: {error}"),
                None,
            ));
        }
    }

    let source_ok = match fs::symlink_metadata(from) {
        Ok(_) => {
            unrestored.push(unrestored_change(
                from,
                from_raw,
                "a new file appeared at the move source; the original is retained".into(),
                Some(backup),
            ));
            false
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match restore_backup(backup, from, warnings) {
                Ok(()) => true,
                Err(error) => {
                    unrestored.push(unrestored_change(
                        from,
                        from_raw,
                        format!("restoring the move source failed: {error}"),
                        Some(backup),
                    ));
                    false
                }
            }
        }
        Err(error) => {
            unrestored.push(unrestored_change(
                from,
                from_raw,
                format!("inspecting the move source failed: {error}"),
                Some(backup),
            ));
            false
        }
    };

    if destination_ok && source_ok {
        rolled_back.push(from.to_path_buf());
    }
}

fn cleanup_staged(staged: &mut [Option<StagedFile>], warnings: &mut Vec<String>) {
    for entry in staged.iter_mut() {
        if let Some(mut file) = entry.take() {
            file.cleanup(warnings, false);
        }
    }
}

/// Remove only the directories this call created, deepest first, and only when
/// they are empty. Never removes pre-existing or externally filled directories.
fn cleanup_dirs(created: &mut Vec<PathBuf>) -> Vec<PathBuf> {
    let mut removed = Vec::new();
    for directory in created.iter().rev() {
        if fs::remove_dir(directory).is_ok() {
            removed.push(directory.clone());
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::apply_patch::parse_patch;
    use crate::tools::apply_patch_fs::{preflight_patch, DiskPlan};
    use std::collections::BTreeMap;

    #[derive(Debug, PartialEq, Eq)]
    enum Node {
        Dir,
        File { bytes: Vec<u8>, mode: Option<u32> },
        Link(PathBuf),
    }

    fn tree(root: &Path) -> BTreeMap<PathBuf, Node> {
        let mut nodes = BTreeMap::new();
        for entry in walkdir::WalkDir::new(root).follow_links(false) {
            let entry = entry.unwrap();
            let path = entry.path().to_path_buf();
            let kind = entry.file_type();
            let node = if kind.is_symlink() {
                Node::Link(fs::read_link(&path).unwrap())
            } else if kind.is_dir() {
                Node::Dir
            } else if kind.is_file() {
                Node::File {
                    bytes: fs::read(&path).unwrap(),
                    mode: mode_of(&path),
                }
            } else {
                panic!("unexpected special file {}", path.display());
            };
            nodes.insert(path, node);
        }
        nodes
    }

    #[cfg(unix)]
    fn mode_of(path: &Path) -> Option<u32> {
        use std::os::unix::fs::PermissionsExt;
        Some(fs::metadata(path).unwrap().permissions().mode() & 0o777)
    }

    #[cfg(not(unix))]
    fn mode_of(_path: &Path) -> Option<u32> {
        None
    }

    #[cfg(unix)]
    fn set_mode(path: &Path, bits: u32) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(bits)).unwrap();
    }

    fn setup() -> (tempfile::TempDir, PathBuf, TrustedRoots) {
        let temp = tempfile::tempdir().unwrap();
        let work = temp.path().join("work");
        fs::create_dir_all(&work).unwrap();
        let roots = TrustedRoots::new(&work, &[]).unwrap();
        (temp, fs::canonicalize(&work).unwrap(), roots)
    }

    fn plan(roots: &TrustedRoots, text: &str) -> DiskPlan {
        preflight_patch(roots, &parse_patch(text).unwrap()).unwrap()
    }

    fn apply(roots: &TrustedRoots, plan: &DiskPlan) -> ApplyReport {
        apply_plan(roots, plan, ApplyHooks::default())
    }

    fn temp_files(root: &Path, kind: &str) -> Vec<PathBuf> {
        let needle = format!(".dcl-patch-{kind}-");
        walkdir::WalkDir::new(root)
            .into_iter()
            .filter_map(Result::ok)
            .map(|entry| entry.path().to_path_buf())
            .filter(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().contains(&needle))
                    .unwrap_or(false)
            })
            .collect()
    }

    #[test]
    fn mixed_patch_applies_exact_bytes_and_modes() {
        let (temp, work, roots) = setup();
        fs::create_dir_all(work.join("sub")).unwrap();
        fs::write(work.join("edit.txt"), "one\ntwo\n").unwrap();
        fs::write(work.join("del.txt"), "delete me\n").unwrap();
        fs::write(work.join("move.txt"), "old\n").unwrap();
        #[cfg(unix)]
        {
            set_mode(&work.join("edit.txt"), 0o755);
            set_mode(&work.join("move.txt"), 0o640);
        }
        let plan = plan(
            &roots,
            "\
*** Begin Patch
*** Add File: sub/new.txt
+added
*** Update File: edit.txt
@@
 one
-two
+TWO
*** Delete File: del.txt
*** Update File: move.txt
*** Move to: moved.txt
@@
-old
+NEW
*** End Patch",
        );

        let report = apply(&roots, &plan);
        assert!(report.succeeded(), "{report:?}");
        assert_eq!(report.applied.len(), 4);
        assert!(report.unchanged.is_empty());
        assert!(report.unrestored.is_empty());
        assert_eq!(fs::read(work.join("sub/new.txt")).unwrap(), b"added\n");
        assert_eq!(fs::read(work.join("edit.txt")).unwrap(), b"one\nTWO\n");
        assert!(!work.join("del.txt").exists());
        assert!(!work.join("move.txt").exists());
        assert_eq!(fs::read(work.join("moved.txt")).unwrap(), b"NEW\n");
        #[cfg(unix)]
        {
            assert_eq!(mode_of(&work.join("edit.txt")), Some(0o755));
            assert_eq!(mode_of(&work.join("moved.txt")), Some(0o640));
            assert_eq!(mode_of(&work.join("sub/new.txt")).unwrap() & 0o111, 0);
        }
        assert!(temp_files(temp.path(), "stage").is_empty());
        assert!(temp_files(temp.path(), "backup").is_empty());
    }

    #[test]
    fn second_file_verification_failure_leaves_every_file_untouched() {
        let (temp, work, roots) = setup();
        fs::write(work.join("a.txt"), "one\n").unwrap();
        fs::write(work.join("b.txt"), "two\n").unwrap();
        let plan = plan(
            &roots,
            "\
*** Begin Patch
*** Update File: a.txt
@@
-one
+ONE
*** Update File: b.txt
@@
-two
+TWO
*** End Patch",
        );
        fs::write(work.join("b.txt"), "external\n").unwrap();
        let snapshot = tree(temp.path());

        let report = apply(&roots, &plan);
        assert!(!report.succeeded());
        match &report.error {
            Some(ApplyError::Verify { path, message, .. }) => {
                assert_eq!(path, &work.join("b.txt"));
                assert!(
                    message.contains("content changed since preflight"),
                    "{message}"
                );
            }
            other => panic!("expected a verification error, got {other:?}"),
        }
        assert!(report.applied.is_empty());
        assert_eq!(tree(temp.path()), snapshot);
        assert!(temp_files(temp.path(), "stage").is_empty());
    }

    #[test]
    fn destination_appearing_after_preflight_is_refused() {
        let (temp, work, roots) = setup();
        let plan = plan(
            &roots,
            "*** Begin Patch\n*** Add File: new.txt\n+created\n*** End Patch",
        );
        fs::write(work.join("new.txt"), "external\n").unwrap();
        let snapshot = tree(temp.path());

        let report = apply(&roots, &plan);
        assert!(!report.succeeded());
        assert!(matches!(report.error, Some(ApplyError::Verify { .. })));
        assert_eq!(fs::read(work.join("new.txt")).unwrap(), b"external\n");
        assert_eq!(tree(temp.path()), snapshot);
    }

    #[cfg(unix)]
    #[test]
    fn source_replaced_by_a_symlink_after_preflight_is_refused() {
        use std::os::unix::fs::symlink;

        let (temp, work, roots) = setup();
        let outside = temp.path().join("outside.txt");
        fs::write(work.join("a.txt"), "old\n").unwrap();
        fs::write(&outside, "outside\n").unwrap();
        let plan = plan(
            &roots,
            "*** Begin Patch\n*** Update File: a.txt\n@@\n-old\n+new\n*** End Patch",
        );
        fs::remove_file(work.join("a.txt")).unwrap();
        symlink(&outside, work.join("a.txt")).unwrap();

        let report = apply(&roots, &plan);
        assert!(!report.succeeded());
        assert!(matches!(report.error, Some(ApplyError::Verify { .. })));
        assert_eq!(fs::read(&outside).unwrap(), b"outside\n");
        assert!(work.join("a.txt").is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn parent_directory_swap_after_preflight_is_refused() {
        use std::os::unix::fs::symlink;

        let (_temp, work, roots) = setup();
        fs::create_dir_all(work.join("sub")).unwrap();
        fs::write(work.join("sub/a.txt"), "old\n").unwrap();
        let plan = plan(
            &roots,
            "*** Begin Patch\n*** Update File: sub/a.txt\n@@\n-old\n+new\n*** End Patch",
        );
        fs::rename(work.join("sub"), work.join("sub-real")).unwrap();
        symlink(work.join("sub-real"), work.join("sub")).unwrap();

        let report = apply(&roots, &plan);
        assert!(!report.succeeded());
        match &report.error {
            Some(ApplyError::Verify { message, .. }) => {
                assert!(
                    message.contains("changed after preflight") || message.contains("symlink"),
                    "{message}"
                );
            }
            other => panic!("expected a verification error, got {other:?}"),
        }
        assert_eq!(fs::read(work.join("sub-real/a.txt")).unwrap(), b"old\n");
    }

    #[cfg(unix)]
    #[test]
    fn staging_failure_is_side_effect_free() {
        let (temp, work, roots) = setup();
        fs::write(work.join("a.txt"), "old\n").unwrap();
        let plan = plan(
            &roots,
            "*** Begin Patch\n*** Update File: a.txt\n@@\n-old\n+new\n*** End Patch",
        );
        set_mode(&work, 0o555);
        let report = apply(&roots, &plan);
        set_mode(&work, 0o755);
        assert!(!report.succeeded());
        assert!(matches!(report.error, Some(ApplyError::Stage { .. })));
        assert_eq!(fs::read(work.join("a.txt")).unwrap(), b"old\n");
        assert!(temp_files(temp.path(), "stage").is_empty());
    }

    #[test]
    fn failure_after_first_commit_rolls_back_applied_change() {
        let (temp, work, roots) = setup();
        fs::write(work.join("a.txt"), "one\n").unwrap();
        fs::write(work.join("b.txt"), "two\n").unwrap();
        #[cfg(unix)]
        set_mode(&work.join("a.txt"), 0o750);
        let plan = plan(
            &roots,
            "\
*** Begin Patch
*** Update File: a.txt
@@
-one
+ONE
*** Update File: b.txt
@@
-two
+TWO
*** End Patch",
        );
        let before_commit = |index: usize| -> Result<(), String> {
            if index == 1 {
                Err("injected commit failure".into())
            } else {
                Ok(())
            }
        };
        let hooks = ApplyHooks {
            before_commit: Some(&before_commit),
            ..Default::default()
        };

        let report = apply_plan(&roots, &plan, hooks);
        assert!(!report.succeeded());
        assert!(matches!(report.error, Some(ApplyError::Hook(_))));
        assert!(report.applied.is_empty(), "{report:?}");
        assert_eq!(report.rolled_back, vec![work.join("a.txt")]);
        assert!(report.unrestored.is_empty(), "{report:?}");
        assert_eq!(fs::read(work.join("a.txt")).unwrap(), b"one\n");
        assert_eq!(fs::read(work.join("b.txt")).unwrap(), b"two\n");
        #[cfg(unix)]
        assert_eq!(mode_of(&work.join("a.txt")), Some(0o750));
        assert!(temp_files(temp.path(), "stage").is_empty());
        assert!(temp_files(temp.path(), "backup").is_empty());
    }

    #[test]
    fn rollback_preserves_external_edit_and_names_unrestored_path() {
        let (temp, work, roots) = setup();
        fs::write(work.join("a.txt"), "one\n").unwrap();
        fs::write(work.join("b.txt"), "two\n").unwrap();
        let plan = plan(
            &roots,
            "\
*** Begin Patch
*** Update File: a.txt
@@
-one
+ONE
*** Update File: b.txt
@@
-two
+TWO
*** End Patch",
        );
        let edited = work.join("a.txt");
        let after_commit = move |index: usize| -> Result<(), String> {
            if index == 0 {
                fs::write(&edited, "external edit\n").unwrap();
            }
            Ok(())
        };
        let before_commit = |index: usize| -> Result<(), String> {
            if index == 1 {
                Err("injected commit failure".into())
            } else {
                Ok(())
            }
        };
        let hooks = ApplyHooks {
            before_commit: Some(&before_commit),
            after_commit: Some(&after_commit),
            ..Default::default()
        };

        let report = apply_plan(&roots, &plan, hooks);
        assert!(!report.succeeded());
        assert_eq!(fs::read(work.join("a.txt")).unwrap(), b"external edit\n");
        assert_eq!(fs::read(work.join("b.txt")).unwrap(), b"two\n");
        assert_eq!(report.unrestored.len(), 1, "{report:?}");
        let unrestored = &report.unrestored[0];
        assert_eq!(unrestored.path, work.join("a.txt"));
        assert!(
            unrestored.message.contains("changed by another process"),
            "{}",
            unrestored.message
        );
        let backup = unrestored.backup.as_ref().expect("backup retained");
        assert_eq!(fs::read(backup).unwrap(), b"one\n");
        assert!(report.rollback_error.is_some());
        assert_eq!(report.rolled_back, Vec::<PathBuf>::new());
        // The still-committed path remains listed as applied and unrestored.
        assert_eq!(report.applied.len(), 1);
        assert_eq!(report.applied[0].path, work.join("a.txt"));
        assert!(temp_files(temp.path(), "stage").is_empty());
        assert_eq!(temp_files(temp.path(), "backup").len(), 1);
    }

    #[test]
    fn missing_parent_directories_are_created_and_reported() {
        let (_temp, work, roots) = setup();
        let plan = plan(
            &roots,
            "*** Begin Patch\n*** Add File: x/y/z.txt\n+deep\n*** End Patch",
        );
        let report = apply(&roots, &plan);
        assert!(report.succeeded(), "{report:?}");
        assert_eq!(fs::read(work.join("x/y/z.txt")).unwrap(), b"deep\n");
        assert!(report.created_dirs.contains(&work.join("x")));
        assert!(report.created_dirs.contains(&work.join("x/y")));
        assert!(report.removed_dirs.is_empty());
    }

    #[test]
    fn failed_apply_removes_only_created_empty_directories() {
        let (_temp, work, roots) = setup();
        fs::create_dir_all(work.join("existing")).unwrap();
        fs::write(work.join("a.txt"), "old\n").unwrap();
        let plan = plan(
            &roots,
            "\
*** Begin Patch
*** Add File: newdir/sub/new.txt
+content
*** Update File: a.txt
@@
-old
+new
*** End Patch",
        );
        let after_commit = |index: usize| -> Result<(), String> {
            if index == 0 {
                Err("injected failure after add".into())
            } else {
                Ok(())
            }
        };
        let hooks = ApplyHooks {
            after_commit: Some(&after_commit),
            ..Default::default()
        };

        let report = apply_plan(&roots, &plan, hooks);
        assert!(!report.succeeded());
        assert!(!work.join("newdir").exists());
        assert!(work.join("existing").is_dir());
        assert_eq!(fs::read(work.join("a.txt")).unwrap(), b"old\n");
        assert!(report.created_dirs.contains(&work.join("newdir")));
        assert!(report.removed_dirs.contains(&work.join("newdir")));
        assert!(report.removed_dirs.contains(&work.join("newdir/sub")));
        assert!(temp_files(work.as_path(), "stage").is_empty());
    }

    #[test]
    fn pure_binary_move_keeps_bytes() {
        let (_temp, work, roots) = setup();
        let binary = vec![0u8, 0xff, 0x80, 0x00, b'\n'];
        fs::write(work.join("bin.dat"), &binary).unwrap();
        #[cfg(unix)]
        set_mode(&work.join("bin.dat"), 0o640);
        let plan = plan(
            &roots,
            "*** Begin Patch\n*** Update File: bin.dat\n*** Move to: renamed.dat\n*** End Patch",
        );
        let report = apply(&roots, &plan);
        assert!(report.succeeded(), "{report:?}");
        assert_eq!(report.applied[0].action, AppliedAction::Moved);
        assert_eq!(
            report.applied[0].destination,
            Some(work.join("renamed.dat"))
        );
        assert_eq!(fs::read(work.join("renamed.dat")).unwrap(), binary);
        assert!(!work.join("bin.dat").exists());
        #[cfg(unix)]
        assert_eq!(mode_of(&work.join("renamed.dat")), Some(0o640));
    }

    #[cfg(unix)]
    #[test]
    fn unchanged_update_is_reported_without_writing() {
        use std::os::unix::fs::MetadataExt;

        let (_temp, work, roots) = setup();
        fs::write(work.join("a.txt"), "same\n").unwrap();
        let inode_before = fs::metadata(work.join("a.txt")).unwrap().ino();
        let plan = plan(
            &roots,
            "*** Begin Patch\n*** Update File: a.txt\n@@\n-same\n+same\n*** End Patch",
        );
        let report = apply(&roots, &plan);
        assert!(report.succeeded(), "{report:?}");
        assert!(report.applied.is_empty());
        assert_eq!(report.unchanged, vec![work.join("a.txt")]);
        assert_eq!(
            fs::metadata(work.join("a.txt")).unwrap().ino(),
            inode_before
        );
        assert_eq!(fs::read(work.join("a.txt")).unwrap(), b"same\n");
    }

    #[test]
    fn install_failure_cleans_staging_and_rolls_back_earlier_changes() {
        let (temp, work, roots) = setup();
        let plan = plan(
            &roots,
            "\
*** Begin Patch
*** Add File: first.txt
+first
*** Add File: second.txt
+second
*** End Patch",
        );
        // Occupy the second destination with a directory immediately before its
        // install, after this change's own revalidation has passed, so the
        // exclusive install fails for a real filesystem reason.
        let blocked = work.join("second.txt");
        let inject = move |index: usize, fault: InstallFault| -> bool {
            if index == 1 && fault == InstallFault::BeforeInstallTarget {
                fs::create_dir(&blocked).unwrap();
            }
            false
        };
        let hooks = ApplyHooks {
            install_fault: Some(&inject),
            ..Default::default()
        };

        let report = apply_plan(&roots, &plan, hooks);
        assert!(!report.succeeded());
        assert!(
            matches!(report.error, Some(ApplyError::Commit { .. })),
            "{report:?}"
        );
        assert!(!work.join("first.txt").exists());
        assert!(work.join("second.txt").is_dir());
        assert!(report.applied.is_empty(), "{report:?}");
        assert_eq!(report.rolled_back, vec![work.join("first.txt")]);
        assert!(temp_files(temp.path(), "stage").is_empty());
        assert!(temp_files(temp.path(), "backup").is_empty());
    }

    #[test]
    fn hardlink_stage_cleanup_failure_is_a_warning_on_committed_install() {
        let (temp, work, roots) = setup();
        let plan = plan(
            &roots,
            "*** Begin Patch\n*** Add File: a.txt\n+new\n*** End Patch",
        );
        let fail_cleanup = |index: usize| index == 0;
        let hooks = ApplyHooks {
            fail_stage_cleanup: Some(&fail_cleanup),
            ..Default::default()
        };
        let report = apply_plan(&roots, &plan, hooks);
        assert!(report.succeeded(), "{report:?}");
        assert_eq!(report.applied.len(), 1);
        assert_eq!(report.applied[0].action, AppliedAction::Added);
        assert_eq!(fs::read(work.join("a.txt")).unwrap(), b"new\n");
        assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
        assert!(
            report.warnings[0].contains("staging file"),
            "{:?}",
            report.warnings
        );
        // The cleanup failure is real: the file is still there and is reported.
        assert_eq!(temp_files(temp.path(), "stage").len(), 1);
        for path in temp_files(temp.path(), "stage") {
            fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn copy_stage_cleanup_failure_is_a_warning_on_committed_install() {
        let (temp, work, roots) = setup();
        let plan = plan(
            &roots,
            "*** Begin Patch\n*** Add File: a.txt\n+copy\n*** End Patch",
        );
        let fail_cleanup = |_index: usize| true;
        let hooks = ApplyHooks {
            force_copy_install: true,
            fail_stage_cleanup: Some(&fail_cleanup),
            ..Default::default()
        };
        let report = apply_plan(&roots, &plan, hooks);
        assert!(report.succeeded(), "{report:?}");
        assert_eq!(fs::read(work.join("a.txt")).unwrap(), b"copy\n");
        assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
        assert_eq!(temp_files(temp.path(), "stage").len(), 1);
        for path in temp_files(temp.path(), "stage") {
            fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn copy_sync_failure_reports_commit_error_and_cleans_staging() {
        let (temp, work, roots) = setup();
        let plan = plan(
            &roots,
            "*** Begin Patch\n*** Add File: a.txt\n+new\n*** End Patch",
        );
        let inject = |_index: usize, fault: InstallFault| fault == InstallFault::CopySync;
        let hooks = ApplyHooks {
            force_copy_install: true,
            install_fault: Some(&inject),
            ..Default::default()
        };
        let report = apply_plan(&roots, &plan, hooks);
        assert!(!report.succeeded());
        match &report.error {
            Some(ApplyError::Commit { path, message }) => {
                assert_eq!(path, &work.join("a.txt"));
                assert!(message.contains("simulated copy sync failure"), "{message}");
            }
            other => panic!("expected a commit error, got {other:?}"),
        }
        assert!(!work.join("a.txt").exists());
        assert!(report.applied.is_empty());
        assert!(temp_files(temp.path(), "stage").is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn copy_permission_failure_on_mode_preserving_update_is_reported() {
        let (temp, work, roots) = setup();
        fs::write(work.join("a.txt"), "old\n").unwrap();
        set_mode(&work.join("a.txt"), 0o755);
        let plan = plan(
            &roots,
            "*** Begin Patch\n*** Update File: a.txt\n@@\n-old\n+new\n*** End Patch",
        );
        let inject = |_index: usize, fault: InstallFault| fault == InstallFault::CopyPermission;
        let hooks = ApplyHooks {
            force_copy_install: true,
            install_fault: Some(&inject),
            ..Default::default()
        };
        let report = apply_plan(&roots, &plan, hooks);
        assert!(!report.succeeded());
        assert!(
            matches!(report.error, Some(ApplyError::Commit { .. })),
            "{report:?}"
        );
        // The copy install was removed and the original backup restored; the
        // failure is never reported as success with an unverified mode.
        assert_eq!(fs::read(work.join("a.txt")).unwrap(), b"old\n");
        assert_eq!(mode_of(&work.join("a.txt")), Some(0o755));
        assert!(report.applied.is_empty());
        assert!(report.unrestored.is_empty(), "{report:?}");
        assert!(temp_files(temp.path(), "stage").is_empty());
        assert!(temp_files(temp.path(), "backup").is_empty());
    }

    #[test]
    fn backup_rename_failure_cleans_staged_file() {
        let (temp, work, roots) = setup();
        fs::write(work.join("a.txt"), "old\n").unwrap();
        let plan = plan(
            &roots,
            "*** Begin Patch\n*** Update File: a.txt\n@@\n-old\n+new\n*** End Patch",
        );
        let inject = |_index: usize, fault: InstallFault| fault == InstallFault::BackupRename;
        let hooks = ApplyHooks {
            install_fault: Some(&inject),
            ..Default::default()
        };
        let report = apply_plan(&roots, &plan, hooks);
        assert!(!report.succeeded());
        assert!(
            matches!(report.error, Some(ApplyError::Commit { .. })),
            "{report:?}"
        );
        assert_eq!(fs::read(work.join("a.txt")).unwrap(), b"old\n");
        assert!(temp_files(temp.path(), "stage").is_empty());
        assert!(temp_files(temp.path(), "backup").is_empty());
    }

    #[test]
    fn install_failure_preserves_external_file_at_target_and_retains_backup() {
        let (temp, work, roots) = setup();
        fs::write(work.join("a.txt"), "old\n").unwrap();
        let plan = plan(
            &roots,
            "*** Begin Patch\n*** Update File: a.txt\n@@\n-old\n+new\n*** End Patch",
        );
        let target = work.join("a.txt");
        let external = target.clone();
        let inject = move |index: usize, fault: InstallFault| -> bool {
            if index == 0 && fault == InstallFault::BeforeInstallTarget {
                // The original has already been renamed to the backup; an
                // external writer recreates the target before the install.
                fs::write(&external, "external\n").unwrap();
            }
            false
        };
        let hooks = ApplyHooks {
            install_fault: Some(&inject),
            ..Default::default()
        };
        let report = apply_plan(&roots, &plan, hooks);
        assert!(!report.succeeded());
        assert!(
            matches!(report.error, Some(ApplyError::Commit { .. })),
            "{report:?}"
        );
        assert_eq!(fs::read(&target).unwrap(), b"external\n");
        assert_eq!(report.unrestored.len(), 1, "{report:?}");
        assert_eq!(report.unrestored[0].path, target);
        assert!(
            report.unrestored[0]
                .message
                .contains("overwrite another file"),
            "{}",
            report.unrestored[0].message
        );
        let backup = report.unrestored[0].backup.as_ref().unwrap();
        assert_eq!(fs::read(backup).unwrap(), b"old\n");
        assert!(report.rollback_error.is_some());
        assert!(temp_files(temp.path(), "stage").is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn rollback_add_detects_external_mode_change() {
        let (_temp, work, roots) = setup();
        let plan = plan(
            &roots,
            "*** Begin Patch\n*** Add File: a.txt\n+new\n*** Add File: b.txt\n+b\n*** End Patch",
        );
        let added = work.join("a.txt");
        let after_commit = move |index: usize| -> Result<(), String> {
            if index == 0 {
                set_mode(&added, 0o600);
            }
            Ok(())
        };
        let before_commit = |index: usize| -> Result<(), String> {
            if index == 1 {
                Err("stop after first change".into())
            } else {
                Ok(())
            }
        };
        let hooks = ApplyHooks {
            before_commit: Some(&before_commit),
            after_commit: Some(&after_commit),
            ..Default::default()
        };
        let report = apply_plan(&roots, &plan, hooks);
        assert!(!report.succeeded());
        assert!(report.rolled_back.is_empty(), "{report:?}");
        assert_eq!(report.unrestored.len(), 1, "{report:?}");
        assert_eq!(report.unrestored[0].path, work.join("a.txt"));
        assert!(
            report.unrestored[0].message.contains("permissions"),
            "{}",
            report.unrestored[0].message
        );
        assert_eq!(fs::read(work.join("a.txt")).unwrap(), b"new\n");
        assert_eq!(mode_of(&work.join("a.txt")), Some(0o600));
        assert!(!work.join("b.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn rollback_update_refuses_symlink_replacement() {
        use std::os::unix::fs::symlink;

        let (_temp, work, roots) = setup();
        fs::write(work.join("a.txt"), "old\n").unwrap();
        fs::write(work.join("b.txt"), "b\n").unwrap();
        fs::write(work.join("outside.txt"), "outside\n").unwrap();
        let plan = plan(
            &roots,
            "*** Begin Patch\n*** Update File: a.txt\n@@\n-old\n+new\n*** Update File: b.txt\n@@\n-b\n+B\n*** End Patch",
        );
        let edited = work.join("a.txt");
        let outside = work.join("outside.txt");
        let after_commit = move |index: usize| -> Result<(), String> {
            if index == 0 {
                fs::remove_file(&edited).unwrap();
                symlink(&outside, &edited).unwrap();
            }
            Ok(())
        };
        let before_commit = |index: usize| -> Result<(), String> {
            if index == 1 {
                Err("stop after first change".into())
            } else {
                Ok(())
            }
        };
        let hooks = ApplyHooks {
            before_commit: Some(&before_commit),
            after_commit: Some(&after_commit),
            ..Default::default()
        };
        let report = apply_plan(&roots, &plan, hooks);
        assert!(!report.succeeded());
        assert_eq!(report.unrestored.len(), 1, "{report:?}");
        assert_eq!(report.unrestored[0].path, work.join("a.txt"));
        assert!(
            report.unrestored[0].message.contains("symbolic link"),
            "{}",
            report.unrestored[0].message
        );
        assert!(work.join("a.txt").is_symlink());
        assert_eq!(fs::read(work.join("outside.txt")).unwrap(), b"outside\n");
        let backup = report.unrestored[0].backup.as_ref().unwrap();
        assert_eq!(fs::read(backup).unwrap(), b"old\n");
    }

    #[test]
    fn later_file_edit_during_earlier_commit_is_refused_and_earlier_rolled_back() {
        let (_temp, work, roots) = setup();
        fs::write(work.join("a.txt"), "one\n").unwrap();
        fs::write(work.join("b.txt"), "two\n").unwrap();
        let plan = plan(
            &roots,
            "*** Begin Patch\n*** Update File: a.txt\n@@\n-one\n+ONE\n*** Update File: b.txt\n@@\n-two\n+TWO\n*** End Patch",
        );
        let second = work.join("b.txt");
        let after_commit = move |index: usize| -> Result<(), String> {
            if index == 0 {
                fs::write(&second, "external\n").unwrap();
            }
            Ok(())
        };
        let hooks = ApplyHooks {
            after_commit: Some(&after_commit),
            ..Default::default()
        };
        let report = apply_plan(&roots, &plan, hooks);
        assert!(!report.succeeded());
        match &report.error {
            Some(ApplyError::Verify { path, message, .. }) => {
                assert_eq!(path, &work.join("b.txt"));
                assert!(
                    message.contains("content changed since preflight"),
                    "{message}"
                );
            }
            other => panic!("expected a verification error, got {other:?}"),
        }
        assert_eq!(fs::read(work.join("a.txt")).unwrap(), b"one\n");
        assert_eq!(fs::read(work.join("b.txt")).unwrap(), b"external\n");
        assert_eq!(report.rolled_back, vec![work.join("a.txt")]);
        assert!(report.applied.is_empty(), "{report:?}");
        assert!(report.unrestored.is_empty(), "{report:?}");
    }

    #[cfg(unix)]
    #[test]
    fn parent_replacement_at_per_change_boundary_is_refused() {
        use std::os::unix::fs::symlink;

        let (_temp, work, roots) = setup();
        fs::create_dir_all(work.join("d1")).unwrap();
        fs::create_dir_all(work.join("d2")).unwrap();
        fs::write(work.join("d1/a.txt"), "one\n").unwrap();
        fs::write(work.join("d2/b.txt"), "two\n").unwrap();
        let plan = plan(
            &roots,
            "*** Begin Patch\n*** Update File: d1/a.txt\n@@\n-one\n+ONE\n*** Update File: d2/b.txt\n@@\n-two\n+TWO\n*** End Patch",
        );
        let moved = work.join("d2");
        let real = work.join("d2-real");
        let after_commit = move |index: usize| -> Result<(), String> {
            if index == 0 {
                fs::rename(&moved, &real).unwrap();
                symlink(&real, &moved).unwrap();
            }
            Ok(())
        };
        let hooks = ApplyHooks {
            after_commit: Some(&after_commit),
            ..Default::default()
        };
        let report = apply_plan(&roots, &plan, hooks);
        assert!(!report.succeeded());
        match &report.error {
            Some(ApplyError::Verify { path, message, .. }) => {
                assert_eq!(path, &work.join("d2/b.txt"));
                assert!(message.contains("changed after preflight"), "{message}");
            }
            other => panic!("expected a verification error, got {other:?}"),
        }
        assert_eq!(fs::read(work.join("d1/a.txt")).unwrap(), b"one\n");
        assert_eq!(fs::read(work.join("d2-real/b.txt")).unwrap(), b"two\n");
        assert_eq!(report.rolled_back, vec![work.join("d1/a.txt")]);
    }

    #[test]
    fn add_commit_removes_target_when_post_install_inspection_fails() {
        let (temp, work, roots) = setup();
        let plan = plan(
            &roots,
            "*** Begin Patch\n*** Add File: a.txt\n+new\n*** End Patch",
        );
        let inject = |_index: usize, fault: InstallFault| fault == InstallFault::PostInstallInspect;
        let hooks = ApplyHooks {
            install_fault: Some(&inject),
            ..Default::default()
        };
        let report = apply_plan(&roots, &plan, hooks);
        assert!(!report.succeeded(), "{report:?}");
        assert!(
            !work.join("a.txt").exists(),
            "the partially installed file must be removed: {report:?}"
        );
        assert_eq!(report.rolled_back, vec![work.join("a.txt")]);
        assert!(report.unrestored.is_empty(), "{report:?}");
        assert!(report.applied.is_empty(), "{report:?}");
        assert!(temp_files(temp.path(), "stage").is_empty());
    }

    #[test]
    fn add_commit_reports_target_when_inspection_and_cleanup_both_fail() {
        let (temp, work, roots) = setup();
        let plan = plan(
            &roots,
            "*** Begin Patch\n*** Add File: a.txt\n+new\n*** End Patch",
        );
        let inject = |_index: usize, fault: InstallFault| {
            matches!(
                fault,
                InstallFault::PostInstallInspect | InstallFault::CleanupRemove
            )
        };
        let hooks = ApplyHooks {
            install_fault: Some(&inject),
            ..Default::default()
        };
        let report = apply_plan(&roots, &plan, hooks);
        assert!(!report.succeeded(), "{report:?}");
        assert!(work.join("a.txt").exists());
        assert_eq!(report.unrestored.len(), 1, "{report:?}");
        assert_eq!(report.unrestored[0].path, work.join("a.txt"));
        assert!(
            report.unrestored[0]
                .message
                .contains("could not be removed"),
            "{}",
            report.unrestored[0].message
        );
        assert!(report.applied.is_empty(), "{report:?}");
        assert_eq!(fs::read(work.join("a.txt")).unwrap(), b"new\n");
        for path in temp_files(temp.path(), "stage") {
            fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn copy_sync_failure_with_cleanup_failure_reports_leftover_target() {
        let (temp, work, roots) = setup();
        let plan = plan(
            &roots,
            "*** Begin Patch\n*** Add File: a.txt\n+new\n*** End Patch",
        );
        let inject = |_index: usize, fault: InstallFault| {
            matches!(fault, InstallFault::CopySync | InstallFault::CleanupRemove)
        };
        let hooks = ApplyHooks {
            force_copy_install: true,
            install_fault: Some(&inject),
            ..Default::default()
        };
        let report = apply_plan(&roots, &plan, hooks);
        assert!(!report.succeeded(), "{report:?}");
        assert!(
            work.join("a.txt").exists(),
            "the partial copy target was left behind and must be reported"
        );
        assert_eq!(report.unrestored.len(), 1, "{report:?}");
        assert_eq!(report.unrestored[0].path, work.join("a.txt"));
        assert_eq!(fs::read(work.join("a.txt")).unwrap(), b"new\n");
        assert!(report.applied.is_empty(), "{report:?}");
        assert!(temp_files(temp.path(), "stage").is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_replacement_before_inspection_is_preserved_with_backup() {
        use std::os::unix::fs::symlink;

        let (temp, work, roots) = setup();
        fs::write(work.join("a.txt"), "old\n").unwrap();
        fs::write(work.join("outside.txt"), "outside\n").unwrap();
        let plan = plan(
            &roots,
            "*** Begin Patch\n*** Update File: a.txt\n@@\n-old\n+new\n*** End Patch",
        );
        let target = work.join("a.txt");
        let outside = work.join("outside.txt");
        let inject = move |_index: usize, fault: InstallFault| {
            if fault == InstallFault::PostInstallInspect {
                fs::remove_file(&target).unwrap();
                symlink(&outside, &target).unwrap();
            }
            false
        };
        let hooks = ApplyHooks {
            install_fault: Some(&inject),
            ..Default::default()
        };
        let report = apply_plan(&roots, &plan, hooks);
        assert!(!report.succeeded(), "{report:?}");
        assert!(
            work.join("a.txt").is_symlink(),
            "the replacement symlink must be preserved: {report:?}"
        );
        assert_eq!(fs::read(work.join("outside.txt")).unwrap(), b"outside\n");
        assert_eq!(report.unrestored.len(), 1, "{report:?}");
        let backup = report.unrestored[0]
            .backup
            .as_ref()
            .expect("the original backup is retained");
        assert_eq!(fs::read(backup).unwrap(), b"old\n");
        assert!(temp_files(temp.path(), "stage").is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn regular_file_replacement_before_inspection_is_reported_not_accepted() {
        let (temp, work, roots) = setup();
        fs::write(work.join("a.txt"), "old\n").unwrap();
        let plan = plan(
            &roots,
            "*** Begin Patch\n*** Update File: a.txt\n@@\n-old\n+new\n*** End Patch",
        );
        let target = work.join("a.txt");
        let inject = move |_index: usize, fault: InstallFault| {
            if fault == InstallFault::PostInstallInspect {
                fs::remove_file(&target).unwrap();
                fs::write(&target, "external\n").unwrap();
            }
            false
        };
        let hooks = ApplyHooks {
            install_fault: Some(&inject),
            ..Default::default()
        };
        let report = apply_plan(&roots, &plan, hooks);
        assert!(!report.succeeded(), "{report:?}");
        assert_eq!(fs::read(work.join("a.txt")).unwrap(), b"external\n");
        assert_eq!(report.unrestored.len(), 1, "{report:?}");
        assert_eq!(report.unrestored[0].path, work.join("a.txt"));
        let backup = report.unrestored[0]
            .backup
            .as_ref()
            .expect("the original backup is retained");
        assert_eq!(fs::read(backup).unwrap(), b"old\n");
        assert!(temp_files(temp.path(), "stage").is_empty());
    }

    #[test]
    fn a_plan_path_outside_the_roots_is_refused() {
        let (_temp, work, roots) = setup();
        let outside = work.parent().unwrap().join("outside.txt");
        let plan = DiskPlan {
            changes: vec![DiskChange::Add {
                path: outside.clone(),
                raw_path: "outside.txt".into(),
                contents: b"x\n".to_vec(),
            }],
        };
        let report = apply(&roots, &plan);
        assert!(!report.succeeded());
        assert!(matches!(report.error, Some(ApplyError::Verify { .. })));
        assert!(!outside.exists());
    }
}
