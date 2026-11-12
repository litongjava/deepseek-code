//! Real-Git fixtures for integration preparation verification.
use super::*;
use crate::super_agent::model::{
    IntegrationReport, IntegrationTargetReport, Phase, RepoTarget, SessionRef, Status, TaskRecord,
    TaskSpec,
};
use std::path::PathBuf;
use std::process::Command;

fn git_run(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(cwd)
        .args(args)
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} in {}: {}",
        cwd.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

struct RepoFixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    worktree: PathBuf,
    common: PathBuf,
    reviewed: String,
    base: String,
    target: String,
}

impl RepoFixture {
    fn new(name: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join(name);
        std::fs::create_dir(&root).unwrap();
        git_run(&root, &["init", "-q", "-b", "main"]);
        git_run(&root, &["config", "user.email", "test@example.invalid"]);
        git_run(&root, &["config", "user.name", "Test"]);
        git_run(&root, &["config", "commit.gpgsign", "false"]);
        std::fs::write(root.join("base.txt"), "base\n").unwrap();
        git_run(&root, &["add", "."]);
        git_run(&root, &["commit", "-q", "-m", "base"]);
        let base = git_run(&root, &["rev-parse", "HEAD"]);
        git_run(&root, &["branch", "develop", &base]);
        let root = root.canonicalize().unwrap();
        let task_id = format!("task-{name}");
        let common = PathBuf::from(git_run(
            &root,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        ));
        let common = common.canonicalize().unwrap();
        let worktree = common.join("desktop-worktrees").join(&task_id);
        git_run(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                &format!("agent/{task_id}"),
                &worktree.to_string_lossy(),
                "main",
            ],
        );
        std::fs::write(worktree.join("reviewed.txt"), "value=1\n").unwrap();
        git_run(&worktree, &["add", "."]);
        git_run(&worktree, &["commit", "-q", "-m", "reviewed work"]);
        let reviewed = git_run(&worktree, &["rev-parse", "HEAD"]);
        let target = base.clone();
        Self {
            _temp: temp,
            root,
            worktree,
            common,
            reviewed,
            base,
            target,
        }
    }

    fn repo(&self, id: &str) -> RepoTarget {
        RepoTarget {
            task_id: "task-001".into(),
            repository_id: id.into(),
            repo_root: self.root.clone(),
            git_common_dir: self.common.clone(),
            target_path: self.root.clone(),
            target_branch: "develop".into(),
            task_branch: "agent/task".into(),
            worktree_path: self.worktree.clone(),
            working_path: self.worktree.clone(),
            base_commit: self.base.clone(),
            submitted_commit: Some(self.reviewed.clone()),
            reviewed_commit: Some(self.reviewed.clone()),
            integrated_commit: None,
            integration_status: Default::default(),
            verification_status: Default::default(),
            source_state: vec![],
        }
    }

    /// Moves the target branch forward in an isolated worktree, so the user's
    /// source checkout is never touched.
    fn advance_target(&self, file: &str, content: &str) -> String {
        let path = self.root.join(".git").join("target-checkout");
        let path = self.common.join("target-checkout");
        let _ = std::fs::remove_dir_all(&path);
        git_run(
            &self.root,
            &["worktree", "add", "-q", &path.to_string_lossy(), "develop"],
        );
        std::fs::write(path.join(file), content).unwrap();
        git_run(&path, &["add", "."]);
        git_run(&path, &["commit", "-q", "-m", "target work"]);
        let head = git_run(&path, &["rev-parse", "HEAD"]);
        git_run(
            &self.root,
            &["worktree", "remove", "--force", &path.to_string_lossy()],
        );
        head
    }

    fn merge_target(&self) -> String {
        git_run(&self.worktree, &["merge", "--no-edit", "develop"]);
        git_run(&self.worktree, &["rev-parse", "HEAD"])
    }

    fn empty_commit(&self, message: &str) -> String {
        git_run(
            &self.worktree,
            &["commit", "-q", "--allow-empty", "-m", message],
        );
        git_run(&self.worktree, &["rev-parse", "HEAD"])
    }

    fn head(&self) -> String {
        git_run(&self.worktree, &["rev-parse", "HEAD"])
    }
}

fn task_with(repos: Vec<RepoTarget>) -> TaskRecord {
    TaskRecord {
        task_id: "task-001".into(),
        pair_id: "pair-001".into(),
        router_session_id: "router".into(),
        title: "Integration".into(),
        spec: TaskSpec {
            title: "Integration".into(),
            workspace: PathBuf::from("/workspace"),
            targets: vec!["/workspace/code".into()],
            goal: "Merge the target".into(),
            requirements: vec!["cargo test".into()],
            acceptance_criteria: vec![],
            constraints: vec![],
            out_of_scope: vec![],
            target_branch: Some("develop".into()),
            repository_target_branches: Default::default(),
        },
        spec_version: 1,
        phase: Phase::Integrating,
        status: Status::Running,
        review_round: 0,
        parent_task_id: None,
        depends_on: vec![],
        coder: SessionRef::default(),
        reviewer: SessionRef::default(),
        cleanup_status: Default::default(),
        permission_policy_version: 1,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
        repositories: repos,
        non_git_targets: vec![],
    }
}

fn report_for(entries: &[(&str, &str, &str)]) -> IntegrationReport {
    IntegrationReport {
        spec_version: 1,
        round: 0,
        targets: entries
            .iter()
            .map(|(id, before, after)| IntegrationTargetReport {
                repository_id: (*id).into(),
                target_branch: "develop".into(),
                before_commit: (*before).into(),
                after_commit: (*after).into(),
                conflict: false,
                tests_passed: true,
                issues: vec![],
            })
            .collect(),
    }
}

#[test]
fn changed_target_tree_requires_a_new_final_review() {
    let fixture = RepoFixture::new("changed");
    let repo = fixture.repo("repo-1");
    let task = task_with(vec![repo.clone()]);
    // The target moves BEFORE the baseline is captured; the baseline then
    // records that newer commit.
    let moved = fixture.advance_target("other.txt", "other\n");
    let baselines = capture_baseline(&task).unwrap();
    assert_eq!(baselines[0].baseline_commit, moved);
    assert_ne!(baselines[0].baseline_commit, fixture.target);

    let merged = fixture.merge_target();
    let report = report_for(&[("repo-1", &moved, &merged)]);
    let outcome = verify_preparation(&task, &report, &baselines).unwrap();

    assert!(outcome.complete(), "{:?}", outcome.failures);
    let verified = &outcome.verified[0];
    assert_eq!(verified.integrated_commit, merged);
    assert!(verified.tree_changed, "{verified:?}");
    assert!(!verified.content_review_reused);
    // The persisted fact points at the exact validated commit.
    assert_eq!(
        verified.integrated_tree,
        tree_of(&repo.working_path, &merged).unwrap()
    );
}

#[test]
fn same_tree_with_a_new_commit_reuses_the_content_review() {
    let fixture = RepoFixture::new("same");
    let repo = fixture.repo("repo-1");
    let task = task_with(vec![repo]);
    let baselines = capture_baseline(&task).unwrap();
    let empty = fixture.empty_commit("no-op");

    let report = report_for(&[("repo-1", &fixture.base, &empty)]);
    let outcome = verify_preparation(&task, &report, &baselines).unwrap();
    assert!(outcome.complete(), "{:?}", outcome.failures);
    let verified = &outcome.verified[0];
    assert_ne!(verified.integrated_commit, fixture.reviewed);
    assert!(!verified.tree_changed);
    assert!(verified.content_review_reused);
    assert!(
        verified.reason.contains("equals the reviewed tree"),
        "{}",
        verified.reason
    );
}

#[test]
fn a_moved_target_baseline_is_rejected_as_stale() {
    let fixture = RepoFixture::new("stale");
    let repo = fixture.repo("repo-1");
    let task = task_with(vec![repo]);
    let baselines = capture_baseline(&task).unwrap();
    // The target moves after the baseline was captured but before the claim.
    fixture.advance_target("late.txt", "late\n");
    let merged = {
        git_run(&fixture.worktree, &["merge", "--no-edit", "develop"]);
        fixture.head()
    };
    let report = report_for(&[("repo-1", &fixture.base, &merged)]);
    let outcome = verify_preparation(&task, &report, &baselines).unwrap();
    assert!(!outcome.complete());
    assert!(outcome.stale_baseline, "{:?}", outcome.failures);
    assert!(
        outcome.failures[0].contains("moved"),
        "{:?}",
        outcome.failures
    );
}

#[test]
fn uncommitted_or_unaligned_claims_are_rejected() {
    let fixture = RepoFixture::new("dirty");
    let repo = fixture.repo("repo-1");
    let task = task_with(vec![repo.clone()]);
    let baselines = capture_baseline(&task).unwrap();

    let bogus = report_for(&[(
        "repo-1",
        &fixture.base,
        "0000000000000000000000000000000000000000",
    )]);
    let outcome = verify_preparation(&task, &bogus, &baselines).unwrap();
    assert!(!outcome.complete());
    assert!(
        outcome.failures[0].contains("HEAD"),
        "{:?}",
        outcome.failures
    );

    let wrong_baseline = report_for(&[("repo-1", "deadbeef", &fixture.reviewed)]);
    let outcome = verify_preparation(&task, &wrong_baseline, &baselines).unwrap();
    assert!(!outcome.complete());
    assert!(
        outcome.failures[0].contains("baseline"),
        "{:?}",
        outcome.failures
    );

    std::fs::write(fixture.worktree.join("reviewed.txt"), "uncommitted\n").unwrap();
    let dirty = report_for(&[("repo-1", &fixture.base, &fixture.head())]);
    let outcome = verify_preparation(&task, &dirty, &baselines).unwrap();
    assert!(!outcome.complete());
    assert!(
        outcome.failures[0].contains("uncommitted"),
        "{:?}",
        outcome.failures
    );
}

#[test]
fn duplicate_or_foreign_repository_identities_are_rejected() {
    let fixture = RepoFixture::new("identity");
    let repo = fixture.repo("repo-1");
    let task = task_with(vec![repo]);
    let baselines = capture_baseline(&task).unwrap();
    let duplicated = report_for(&[
        ("repo-1", &fixture.base, &fixture.reviewed),
        ("repo-1", &fixture.base, &fixture.reviewed),
    ]);
    let error = verify_preparation(&task, &duplicated, &baselines).unwrap_err();
    assert!(format!("{error:#}").contains("more than once"), "{error:#}");

    let foreign = report_for(&[
        ("repo-1", &fixture.base, &fixture.reviewed),
        ("repo-2", &fixture.base, &fixture.reviewed),
    ]);
    let error = verify_preparation(&task, &foreign, &baselines).unwrap_err();
    assert!(
        format!("{error:#}").contains("not part of this task"),
        "{error:#}"
    );
}

#[test]
fn one_repository_failure_preserves_the_verified_one() {
    let first = RepoFixture::new("first");
    let second = RepoFixture::new("second");
    let task = task_with(vec![first.repo("repo-1"), second.repo("repo-2")]);
    let baselines = capture_baseline(&task).unwrap();
    let report = report_for(&[
        ("repo-1", &first.base, &first.reviewed),
        (
            "repo-2",
            &second.base,
            "0000000000000000000000000000000000000000",
        ),
    ]);
    let outcome = verify_preparation(&task, &report, &baselines).unwrap();
    assert!(!outcome.complete());
    assert_eq!(outcome.verified.len(), 1);
    assert_eq!(outcome.verified[0].repository_id, "repo-1");
    assert_eq!(outcome.failures.len(), 1);
    assert!(
        outcome.failures[0].starts_with("repo-2:"),
        "{:?}",
        outcome.failures
    );
}

#[test]
fn a_head_change_after_validation_is_never_promoted() {
    let fixture = RepoFixture::new("race");
    let repo = fixture.repo("repo-1");
    let task = task_with(vec![repo]);
    let baselines = capture_baseline(&task).unwrap();
    let report = report_for(&[("repo-1", &fixture.base, &fixture.reviewed)]);
    let worktree = fixture.worktree.clone();
    let mut hook = |repo: &RepoTarget| {
        // The worktree moves after the validated commit was read, before the
        // fact is captured.
        git_run(
            &worktree,
            &["commit", "-q", "--allow-empty", "-m", "racing"],
        );
        assert_ne!(
            git_run(&repo.working_path, &["rev-parse", "HEAD"]),
            report.targets[0].after_commit
        );
    };
    let outcome = verify_preparation_observed(&task, &report, &baselines, &mut hook).unwrap();
    assert!(outcome.complete(), "{:?}", outcome.failures);
    assert_eq!(outcome.verified[0].integrated_commit, fixture.reviewed);
    assert_ne!(outcome.verified[0].integrated_commit, fixture.head());
}

#[test]
fn a_changed_gitlink_requires_a_new_review_even_when_diff_ignores_submodules() {
    let fixture = RepoFixture::new("gitlink");
    let sub = RepoFixture::new("submodule-source");
    // The gitlink must be part of the reviewed commit, so rewrite the task
    // branch with the submodule and use that as the reviewed version.
    git_run(
        &fixture.worktree,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            &sub.root.to_string_lossy(),
            "sub",
        ],
    );
    git_run(&fixture.worktree, &["commit", "-q", "-am", "add submodule"]);
    let reviewed = fixture.head();
    // A new commit in the submodule becomes a changed gitlink.
    std::fs::write(sub.root.join("base.txt"), "moved\n").unwrap();
    git_run(&sub.root, &["commit", "-q", "-am", "submodule moved"]);
    let sub_head = git_run(&sub.root, &["rev-parse", "HEAD"]);
    git_run(
        &fixture.worktree,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "update",
            "--init",
            "--remote",
            "sub",
        ],
    );
    // Force the gitlink to the new submodule commit without a checked-out diff.
    git_run(
        &fixture.worktree,
        &[
            "update-index",
            "--cacheinfo",
            &format!("160000,{sub_head},sub"),
        ],
    );
    git_run(&fixture.worktree, &["commit", "-q", "-m", "move gitlink"]);
    let integrated = fixture.head();
    // Diff output is suppressed by configuration, but the tree objects differ.
    git_run(
        &fixture.worktree,
        &["config", "diff.ignoreSubmodules", "all"],
    );
    assert!(
        git_run(
            &fixture.worktree,
            &["diff", "--name-only", &reviewed, &integrated]
        )
        .is_empty(),
        "the fixture must reproduce the suppression"
    );

    let mut repo = fixture.repo("repo-1");
    repo.reviewed_commit = Some(reviewed.clone());
    repo.submitted_commit = Some(reviewed.clone());
    let task = task_with(vec![repo]);
    let baselines = vec![IntegrationBaseline {
        repository_id: "repo-1".into(),
        target_branch: "develop".into(),
        target_ref: "refs/heads/develop".into(),
        baseline_commit: fixture.base.clone(),
        reviewed_commit: reviewed.clone(),
    }];
    let report = report_for(&[("repo-1", &fixture.base, &integrated)]);
    let outcome = verify_preparation(&task, &report, &baselines).unwrap();
    assert!(outcome.complete(), "{:?}", outcome.failures);
    assert!(
        outcome.verified[0].tree_changed,
        "a changed gitlink must require a new review: {:?}",
        outcome.verified[0]
    );
    assert!(!outcome.verified[0].content_review_reused);
}

#[test]
fn a_dirty_source_state_is_preserved_and_checked() {
    let fixture = RepoFixture::new("source");
    std::fs::write(fixture.root.join("base.txt"), "user edit\n").unwrap();
    let mut repo = fixture.repo("repo-1");
    repo.source_state = vec![" M base.txt".into()];
    let task = task_with(vec![repo]);
    let baselines = capture_baseline(&task).unwrap();
    let report = report_for(&[("repo-1", &fixture.base, &fixture.reviewed)]);
    let outcome = verify_preparation(&task, &report, &baselines).unwrap();
    assert!(outcome.complete(), "{:?}", outcome.failures);

    // Reverting the user's own change is reported, not silently accepted.
    git_run(&fixture.root, &["checkout", "--", "base.txt"]);
    let outcome = verify_preparation(&task, &report, &baselines).unwrap();
    assert!(!outcome.complete());
    assert!(
        outcome.failures[0].contains("original source change"),
        "{:?}",
        outcome.failures
    );
}
