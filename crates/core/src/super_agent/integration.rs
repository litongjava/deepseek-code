//! Stage5A: integration preparation inside the Coder's isolated task worktree.
//!
//! The Coder merges the target reference into its own task worktree and submits
//! an `integration` report. Every field of that report is a **claim**. This
//! module verifies the claim against real Git state and produces the durable
//! preparation facts the host records. It never changes the user's target
//! branch, so it can never report delivery.
use crate::super_agent::model::{
    IntegrationReport, IntegrationTargetReport, RepoTarget, TaskRecord,
};
use crate::worktree::session::{command, git};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// The target state captured immediately before the Coder is instructed. A
/// later move of the target makes this baseline stale and the claim invalid.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationBaseline {
    pub repository_id: String,
    pub target_branch: String,
    pub target_ref: String,
    pub baseline_commit: String,
    pub reviewed_commit: String,
}

/// Host-verified preparation of one repository. Only this, not the Coder's
/// report, may be recorded as a durable preparation fact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifiedIntegration {
    pub repository_id: String,
    pub target_branch: String,
    pub target_ref: String,
    pub baseline_commit: String,
    pub reviewed_commit: String,
    pub integrated_commit: String,
    pub integrated_tree: String,
    /// The integrated content differs from the reviewed content, so a new
    /// final-delivery review is required.
    pub tree_changed: bool,
    /// True when the integrated tree equals the reviewed tree, so the existing
    /// content review still applies.
    pub content_review_reused: bool,
    /// Human-readable reason recorded with the preparation fact.
    pub reason: String,
}

/// The result of verifying a preparation claim. Verified repositories keep
/// their facts even when another repository fails, but no preparation fact or
/// phase advance may be published for a partial result.
#[derive(Debug, Default)]
pub struct PreparationOutcome {
    pub verified: Vec<VerifiedIntegration>,
    pub failures: Vec<String>,
    /// At least one target reference moved after the baseline was captured.
    pub stale_baseline: bool,
}

impl PreparationOutcome {
    pub fn complete(&self) -> bool {
        self.failures.is_empty() && !self.verified.is_empty()
    }

    pub fn tree_changed(&self) -> bool {
        self.verified.iter().any(|repo| repo.tree_changed)
    }
}

pub(crate) fn resolve_ref(repo: &RepoTarget, reference: &str) -> Result<String> {
    let commit = git(
        &repo.repo_root,
        &["rev-parse", "--verify", &format!("{reference}^{{commit}}")],
    )
    .with_context(|| format!("Cannot resolve {reference} in {}", repo.repo_root.display()))?;
    ensure!(!commit.is_empty(), "{reference} is empty");
    Ok(commit)
}

fn common_dir(repo: &RepoTarget) -> Result<String> {
    git(
        &repo.working_path,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
}

/// Porcelain entries with the same encoding `discovery::source_state` records,
/// so a captured source change can be compared exactly. Observation must not
/// refresh the user's index (including during completed-task cleanup).
pub(crate) fn status_entries(path: &Path) -> Result<Vec<String>> {
    let output = command(
        path,
        &[
            "--no-optional-locks",
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
        ],
    )?;
    ensure!(
        output.status.success(),
        "Cannot read the status of {}: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&output.stdout)
        .split('\0')
        .filter(|entry| !entry.is_empty())
        .map(str::to_owned)
        .collect())
}

pub(crate) fn is_ancestor(path: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
    let output = command(path, &["merge-base", "--is-ancestor", ancestor, descendant])?;
    match output.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        _ => anyhow::bail!(
            "Cannot compare {ancestor} with {descendant} in {}: {}",
            path.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        ),
    }
}

fn changed_paths(path: &Path, from: &str, to: &str) -> Result<Vec<String>> {
    let output = git(path, &["diff", "--name-only", from, to])?;
    Ok(output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect())
}

/// Resolves the current target reference for every Git target. Called before
/// the Coder is told to merge, so a later move is detectable.
pub fn capture_baseline(task: &TaskRecord) -> Result<Vec<IntegrationBaseline>> {
    let mut baselines = Vec::new();
    for repo in &task.repositories {
        let reviewed = repo
            .reviewed_commit
            .as_deref()
            .or(repo.submitted_commit.as_deref())
            .context("The task has no reviewed commit to integrate")?;
        let target_ref = format!("refs/heads/{}", repo.target_branch);
        let baseline_commit = resolve_ref(repo, &target_ref)?;
        baselines.push(IntegrationBaseline {
            repository_id: repo.repository_id.clone(),
            target_branch: repo.target_branch.clone(),
            target_ref,
            baseline_commit,
            reviewed_commit: reviewed.to_owned(),
        });
    }
    ensure!(
        !baselines.is_empty(),
        "The task has no Git target to integrate"
    );
    Ok(baselines)
}

/// Whether the captured target baseline still matches reality.
pub fn baseline_moved(baseline: &IntegrationBaseline, repo: &RepoTarget) -> Result<Option<String>> {
    let current = resolve_ref(repo, &baseline.target_ref)?;
    if current == baseline.baseline_commit {
        return Ok(None);
    }
    Ok(Some(format!(
        "target branch {} moved from {} to {} during integration preparation; the captured baseline is stale and a new integration is required",
        baseline.target_branch, baseline.baseline_commit, current
    )))
}

/// Verifies every claimed preparation against the repository's actual state.
///
/// A failure never discards the repositories that did verify, but the caller
/// must not publish a preparation fact or advance the phase for a partial
/// result.
pub fn verify_preparation(
    task: &TaskRecord,
    report: &IntegrationReport,
    baselines: &[IntegrationBaseline],
) -> Result<PreparationOutcome> {
    verify_preparation_observed(task, report, baselines, &mut |_| {})
}

/// Same as [`verify_preparation`] with an observation hook that runs after a
/// repository was validated and before its facts are captured. Production
/// passes a no-op; tests use it to prove that a HEAD change in that window is
/// never promoted as the verified version.
pub fn verify_preparation_observed<F: FnMut(&RepoTarget)>(
    task: &TaskRecord,
    report: &IntegrationReport,
    baselines: &[IntegrationBaseline],
    after_validation: &mut F,
) -> Result<PreparationOutcome> {
    ensure!(
        !report.targets.is_empty(),
        "An integration report must describe at least one repository"
    );
    let mut ids = BTreeSet::new();
    for target in &report.targets {
        ensure!(
            ids.insert(target.repository_id.as_str()),
            "The integration report lists repository {} more than once",
            target.repository_id
        );
    }
    let known: BTreeSet<&str> = task
        .repositories
        .iter()
        .map(|repo| repo.repository_id.as_str())
        .collect();
    let foreign: Vec<&str> = report
        .targets
        .iter()
        .map(|target| target.repository_id.as_str())
        .filter(|id| !known.contains(id))
        .collect();
    ensure!(
        foreign.is_empty(),
        "The integration report names repositories that are not part of this task: {foreign:?}"
    );

    let mut outcome = PreparationOutcome::default();
    for repo in &task.repositories {
        let Some(baseline) = baselines
            .iter()
            .find(|baseline| baseline.repository_id == repo.repository_id)
        else {
            outcome.failures.push(format!(
                "{}: no integration baseline was captured",
                repo.repository_id
            ));
            continue;
        };
        let Some(target) = report
            .targets
            .iter()
            .find(|target| target.repository_id == repo.repository_id)
        else {
            outcome.failures.push(format!(
                "{}: the integration report does not describe this repository",
                repo.repository_id
            ));
            continue;
        };
        match verify_repository(repo, baseline, target) {
            Ok((head, tree)) => {
                // All facts below come from the commit that was actually
                // validated. A later HEAD change is not promoted.
                after_validation(repo);
                let reviewed_tree = tree_of(&repo.working_path, &baseline.reviewed_commit)?;
                // Tree object identity, not diff output: `diff.ignoreSubmodules`
                // can hide a changed gitlink, but the tree OIDs still differ.
                let tree_changed = reviewed_tree != tree;
                let changed = if tree_changed {
                    changed_paths(&repo.working_path, &baseline.reviewed_commit, &head)?
                } else {
                    Vec::new()
                };
                outcome.verified.push(VerifiedIntegration {
                    repository_id: repo.repository_id.clone(),
                    target_branch: repo.target_branch.clone(),
                    target_ref: baseline.target_ref.clone(),
                    baseline_commit: baseline.baseline_commit.clone(),
                    reviewed_commit: baseline.reviewed_commit.clone(),
                    integrated_commit: head,
                    integrated_tree: tree,
                    tree_changed,
                    content_review_reused: !tree_changed,
                    reason: if tree_changed {
                        if changed.is_empty() {
                            "the integrated tree object differs from the reviewed tree".into()
                        } else {
                            format!(
                                "the integrated content differs from the reviewed content in {} path(s)",
                                changed.len()
                            )
                        }
                    } else {
                        "the integrated tree equals the reviewed tree; the existing content review still applies".into()
                    },
                });
            }
            Err(error) => {
                if baseline_moved(baseline, repo)?.is_some() {
                    outcome.stale_baseline = true;
                }
                outcome
                    .failures
                    .push(format!("{}: {error:#}", repo.repository_id));
            }
        }
    }
    Ok(outcome)
}

pub(crate) fn tree_of(path: &Path, commit: &str) -> Result<String> {
    git(path, &["rev-parse", &format!("{commit}^{{tree}}")])
}

fn verify_repository(
    repo: &RepoTarget,
    baseline: &IntegrationBaseline,
    target: &IntegrationTargetReport,
) -> Result<(String, String)> {
    ensure!(
        target.target_branch == repo.target_branch,
        "The integration report names target branch {} but this task targets {}",
        target.target_branch,
        repo.target_branch
    );
    // The baseline recorded before the Coder merged must still be the real
    // target. A moved target invalidates the claim instead of retrying.
    if let Some(reason) = baseline_moved(baseline, repo)? {
        anyhow::bail!("{reason}");
    }
    ensure!(
        target.before_commit.trim() == baseline.baseline_commit,
        "The integration report claims baseline {} but the captured baseline is {}",
        target.before_commit.trim(),
        baseline.baseline_commit
    );
    // Repository identity: the worktree must belong to the task's repository.
    let common = PathBuf::from(common_dir(repo)?);
    let common = common.canonicalize().unwrap_or(common);
    let expected = repo
        .git_common_dir
        .canonicalize()
        .unwrap_or_else(|_| repo.git_common_dir.clone());
    ensure!(
        common == expected,
        "The task worktree belongs to {}, expected {}",
        common.display(),
        expected.display()
    );
    // The user's source working tree must not have been cleaned or switched.
    let source = status_entries(&repo.repo_root)?;
    for entry in &repo.source_state {
        ensure!(
            source.iter().any(|line| line == entry),
            "The original source change {entry:?} is no longer present in {}",
            repo.repo_root.display()
        );
    }
    let worktree = status_entries(&repo.working_path)?;
    ensure!(
        worktree.is_empty(),
        "The task worktree {} has uncommitted changes; commit the integration before submitting",
        repo.working_path.display()
    );
    let head = git(&repo.working_path, &["rev-parse", "HEAD"])?;
    ensure!(
        !head.is_empty(),
        "The task worktree {} has no committed HEAD",
        repo.working_path.display()
    );
    ensure!(
        target.after_commit.trim() == head,
        "The integration report claims commit {} but the task worktree HEAD is {}",
        target.after_commit.trim(),
        head
    );
    ensure!(
        !target.conflict,
        "The integration report still reports an unresolved conflict in {}",
        repo.repository_id
    );
    ensure!(
        is_ancestor(&repo.working_path, &baseline.reviewed_commit, &head)?,
        "The reviewed commit {} is not an ancestor of the integrated commit {}",
        baseline.reviewed_commit,
        head
    );
    ensure!(
        is_ancestor(&repo.working_path, &baseline.baseline_commit, &head)?,
        "The integration baseline {} is not an ancestor of the integrated commit {}; the target content was not merged",
        baseline.baseline_commit,
        head
    );
    // Return the exact verified commit and tree. The caller records these,
    // never a later re-read HEAD.
    let tree = tree_of(&repo.working_path, &head)?;
    Ok((head, tree))
}

/// Names the immutable final-delivery checkout for one integrated commit.
pub fn prepare_integrated_checkout(
    repo: &RepoTarget,
    commit: &str,
    task_id: &str,
    spec_version: u32,
    round: u32,
) -> Result<PathBuf> {
    let worktree = crate::worktree::session::prepare_pinned_review_worktree(
        &repo.repo_root,
        commit,
        task_id,
        &format!("final-s{spec_version}-r{round}"),
    )?;
    ensure!(
        worktree.base_commit == commit,
        "The final review checkout is not pinned at the integrated commit"
    );
    Ok(worktree.worktree_root)
}

#[cfg(test)]
mod tests;
