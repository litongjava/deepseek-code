//! Stage 5B1 delivery preflight tests on real temporary Git repositories.
//!
//! Every fixture mutates only its own temporary repository. Test code may move
//! a target directly with Git to model an external process; the product
//! preflight never does.
use super::*;
use crate::super_agent::model::{
    AcceptanceCriterion, EventType, IntegrationStatus, NewEvent, NonGitStatus, NonGitTarget,
    ReportKind, TaskSpec, VerificationStatus,
};
use serde_json::{json, Value};
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
    name: String,
    root: PathBuf,
    worktree: PathBuf,
    common: PathBuf,
    base: String,
    reviewed: String,
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
        let common = PathBuf::from(git_run(
            &root,
            &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        ))
        .canonicalize()
        .unwrap();
        let worktree = common
            .join("desktop-worktrees")
            .join(format!("task-{name}"));
        git_run(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                &format!("agent/{name}"),
                &worktree.to_string_lossy(),
                "main",
            ],
        );
        std::fs::write(worktree.join("reviewed.txt"), "value=1\n").unwrap();
        git_run(&worktree, &["add", "."]);
        git_run(&worktree, &["commit", "-q", "-m", "reviewed work"]);
        let reviewed = git_run(&worktree, &["rev-parse", "HEAD"]);
        Self {
            _temp: temp,
            name: name.to_owned(),
            root,
            worktree,
            common,
            base,
            reviewed,
        }
    }

    fn integrate(&self) -> String {
        git_run(&self.worktree, &["merge", "--no-edit", "develop"]);
        git_run(&self.worktree, &["rev-parse", "HEAD"])
    }

    fn tree_of(&self, commit: &str) -> String {
        git_run(&self.root, &["rev-parse", &format!("{commit}^{{tree}}")])
    }

    /// Commits on the target branch from an isolated worktree. This models an
    /// unrelated external process; it is not a product operation.
    fn advance_develop(&self, file: &str, content: &str) -> String {
        let path = self
            .common
            .join(format!("advance-{}", uuid::Uuid::new_v4()));
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

    /// A distinct target commit that is not the integrated one.
    fn move_target_elsewhere(&self) -> String {
        let path = self.common.join(format!("move-{}", uuid::Uuid::new_v4()));
        git_run(
            &self.root,
            &["worktree", "add", "-q", &path.to_string_lossy(), "develop"],
        );
        git_run(&path, &["commit", "-q", "--allow-empty", "-m", "elsewhere"]);
        let head = git_run(&path, &["rev-parse", "HEAD"]);
        git_run(
            &self.root,
            &["worktree", "remove", "--force", &path.to_string_lossy()],
        );
        head
    }

    fn set_target(&self, oid: &str) {
        git_run(&self.root, &["update-ref", "refs/heads/develop", oid]);
    }

    fn target(&self) -> String {
        git_run(&self.root, &["rev-parse", "refs/heads/develop"])
    }

    /// A linked worktree whose path contains spaces, checked out on develop.
    fn checkout_target(&self) -> PathBuf {
        let path = self.common.join(format!("target checkout {}", self.name));
        git_run(
            &self.root,
            &["worktree", "add", "-q", &path.to_string_lossy(), "develop"],
        );
        path.canonicalize().unwrap()
    }
}

struct RepoData {
    _fixture: RepoFixture,
    repo: RepoTarget,
    entry: VerifiedIntegration,
}

struct Fixture {
    _temp: tempfile::TempDir,
    data: PathBuf,
    store: TaskStore,
    task: TaskRecord,
    repos: Vec<RepoData>,
    changed: bool,
    decision: Option<(ReviewStage, ReviewVerdict)>,
}

impl Fixture {
    fn new() -> Self {
        Self::with(1, false, None)
    }

    fn with(
        repo_count: usize,
        changed: bool,
        decision: Option<(ReviewStage, ReviewVerdict)>,
    ) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let paths = crate::runtime::RuntimePaths::new(data.clone()).unwrap();
        let store = TaskStore::new(paths.open_database().unwrap(), data.clone());
        let fixtures: Vec<RepoFixture> = (0..repo_count)
            .map(|index| RepoFixture::new(&format!("repo-{index}")))
            .collect();
        let spec = TaskSpec {
            title: "Delivery".into(),
            workspace: temp.path().to_path_buf(),
            targets: fixtures
                .iter()
                .map(|fixture| fixture.root.to_string_lossy().into_owned())
                .collect(),
            goal: "Deliver the reviewed content".into(),
            requirements: vec![],
            acceptance_criteria: vec![AcceptanceCriterion {
                id: "AC-001".into(),
                description: "The reviewed value is present".into(),
            }],
            constraints: vec![],
            out_of_scope: vec![],
            target_branch: Some("develop".into()),
            repository_target_branches: Default::default(),
        };
        let task = store.create_task("router", spec, "create").unwrap();
        let mut repos = Vec::new();
        for (index, fixture) in fixtures.into_iter().enumerate() {
            let baseline = if changed {
                fixture.advance_develop("target.txt", "target work\n")
            } else {
                fixture.target()
            };
            let integrated = fixture.integrate();
            let integrated_tree = fixture.tree_of(&integrated);
            let repository_id = format!("repo-{}", index + 1);
            let repo = RepoTarget {
                task_id: task.task_id.clone(),
                repository_id: repository_id.clone(),
                repo_root: fixture.root.clone(),
                git_common_dir: fixture.common.clone(),
                target_path: fixture.root.clone(),
                target_branch: "develop".into(),
                task_branch: format!("agent/{}", fixture.name),
                worktree_path: fixture.worktree.clone(),
                working_path: fixture.worktree.clone(),
                base_commit: fixture.base.clone(),
                submitted_commit: Some(fixture.reviewed.clone()),
                reviewed_commit: Some(fixture.reviewed.clone()),
                integrated_commit: Some(integrated.clone()),
                integration_status: IntegrationStatus::Prepared,
                verification_status: VerificationStatus::Passed,
                source_state: vec![],
            };
            store.save_repo(&repo).unwrap();
            let entry = VerifiedIntegration {
                repository_id,
                target_branch: "develop".into(),
                target_ref: "refs/heads/develop".into(),
                baseline_commit: baseline,
                reviewed_commit: fixture.reviewed.clone(),
                integrated_commit: integrated,
                integrated_tree,
                tree_changed: changed,
                content_review_reused: !changed,
                reason: "fixture preparation".into(),
            };
            repos.push(RepoData {
                _fixture: fixture,
                repo,
                entry,
            });
        }
        record_evidence(&store, &task, &repos, changed, decision);
        let task = store.get_task(&task.task_id).unwrap().unwrap();
        Self {
            _temp: temp,
            data,
            store,
            task,
            repos,
            changed,
            decision,
        }
    }

    fn plan(&self) -> DeliveryPlan {
        match prepare_delivery(&self.store, &self.task).unwrap() {
            DeliveryPreflight::Planned(plan) => plan,
            other => panic!("expected a planned delivery, got {other:?}"),
        }
    }

    fn competing_task(&self) -> TaskRecord {
        let task = self
            .store
            .create_task("router", self.task.spec.clone(), "create-competing")
            .unwrap();
        for data in &self.repos {
            let mut repo = data.repo.clone();
            repo.task_id = task.task_id.clone();
            self.store.save_repo(&repo).unwrap();
        }
        record_evidence(&self.store, &task, &self.repos, self.changed, self.decision);
        self.store.get_task(&task.task_id).unwrap().unwrap()
    }

    fn reopened(&self) -> TaskStore {
        let paths = crate::runtime::RuntimePaths::new(self.data.clone()).unwrap();
        TaskStore::new(paths.open_database().unwrap(), self.data.clone())
    }
}

fn record_evidence(
    store: &TaskStore,
    task: &TaskRecord,
    repos: &[RepoData],
    changed: bool,
    decision: Option<(ReviewStage, ReviewVerdict)>,
) {
    let (stage, verdict) = decision.unwrap_or((
        if changed {
            ReviewStage::FinalDelivery
        } else {
            ReviewStage::Implementation
        },
        ReviewVerdict::Approved,
    ));
    let reviewed_versions: Vec<Value> = repos
        .iter()
        .map(|data| {
            let commit = if stage == ReviewStage::FinalDelivery {
                data.repo.integrated_commit.clone().unwrap()
            } else {
                data.repo.reviewed_commit.clone().unwrap()
            };
            json!({"repository_id": data.repo.repository_id, "commit": commit})
        })
        .collect();
    let review = store
        .add_report(
            &format!("{}-review", task.task_id),
            &task.task_id,
            ReportKind::Review,
            task.spec_version,
            task.review_round,
            json!({
                "verdict": verdict.as_str(),
                "spec_version": task.spec_version,
                "round": task.review_round,
                "stage": stage.as_str(),
                "reviewed_versions": reviewed_versions,
                "acceptance_results": [],
            }),
        )
        .unwrap();
    let harness = store
        .add_report(
            &format!("{}-harness", task.task_id),
            &task.task_id,
            ReportKind::Harness,
            task.spec_version,
            task.review_round,
            json!({
                "spec_version": task.spec_version,
                "round": task.review_round,
                "stage": stage.as_str(),
                "findings": [],
            }),
        )
        .unwrap();
    let entries: Vec<Value> = repos
        .iter()
        .map(|data| serde_json::to_value(&data.entry).unwrap())
        .collect();
    store
        .ingest_event(
            &task.task_id,
            NewEvent {
                session_id: None,
                run_id: None,
                event_type: EventType::IntegrationPrepared,
                causation_id: None,
                operation_id: format!("{}-prepared", task.task_id),
                payload: json!({"repositories": entries, "tree_changed": changed}),
            },
        )
        .unwrap();
    store
        .ingest_event(
            &task.task_id,
            NewEvent {
                session_id: None,
                run_id: None,
                event_type: EventType::ReviewCompleted,
                causation_id: None,
                operation_id: format!("{}-decision", task.task_id),
                payload: json!({
                    "verdict": verdict.as_str(),
                    "stage": stage.as_str(),
                    "review_report_id": review.report_id,
                    "harness_report_id": harness.report_id,
                    "harness_blocked": false,
                }),
            },
        )
        .unwrap();
}

fn blocked_reason(preflight: DeliveryPreflight) -> String {
    match preflight {
        DeliveryPreflight::Blocked { reason } => reason,
        other => panic!("expected a blocked delivery, got {other:?}"),
    }
}

#[test]
fn a_non_main_target_gets_a_cas_plan_without_touching_git() {
    let fixture = Fixture::new();
    let plan = fixture.plan();
    let target = &plan.repositories[0];
    assert_eq!(target.target_branch, "develop");
    assert_eq!(target.target_ref, "refs/heads/develop");
    assert_eq!(target.destination, DeliveryDestination::RefOnly);
    assert_eq!(target.old_oid, fixture.repos[0].entry.baseline_commit);
    assert_eq!(target.new_oid, fixture.repos[0].entry.integrated_commit);
    assert_ne!(target.old_oid, target.new_oid);
    assert_eq!(target.review_stage, ReviewStage::Implementation);
    assert_eq!(plan.task_id, fixture.task.task_id);
    assert_eq!(plan.spec_version, 1);
    assert_eq!(plan.review_round, 0);

    // Persisted and owned.
    assert_eq!(
        fixture
            .store
            .delivery_plan(&fixture.task.task_id, &plan.operation_id)
            .unwrap(),
        Some(plan.clone())
    );
    assert_eq!(
        fixture.store.locks(&fixture.task.task_id).unwrap(),
        vec![target.lock_key.clone()]
    );

    // A plan changes no Git ref and no worktree file.
    assert_eq!(fixture.repos[0]._fixture.target(), target.old_oid);
    assert_eq!(
        git_run(&fixture.repos[0]._fixture.root, &["rev-parse", "main"]),
        fixture.repos[0]._fixture.base
    );
    assert_eq!(
        std::fs::read_to_string(fixture.repos[0]._fixture.root.join("base.txt")).unwrap(),
        "base\n"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.repos[0]._fixture.worktree.join("reviewed.txt")).unwrap(),
        "value=1\n"
    );
}

#[test]
fn an_empty_target_branch_is_rejected_instead_of_defaulting_to_main() {
    let fixture = Fixture::new();
    let mut task = fixture.task.clone();
    task.repositories[0].target_branch = String::new();
    let reason = blocked_reason(prepare_delivery(&fixture.store, &task).unwrap());
    assert!(reason.contains("never defaults to main"), "{reason}");
    assert!(fixture.store.locks(&task.task_id).unwrap().is_empty());
}

#[test]
fn a_checked_out_target_in_a_linked_worktree_is_the_destination() {
    let fixture = Fixture::new();
    let checkout = fixture.repos[0]._fixture.checkout_target();
    assert!(checkout.to_string_lossy().contains(' '));
    let plan = fixture.plan();
    let target = &plan.repositories[0];
    assert_eq!(
        target.destination,
        DeliveryDestination::CheckedOut {
            path: checkout.clone()
        }
    );
    assert_eq!(
        git_run(&checkout, &["rev-parse", "HEAD"]),
        fixture.repos[0]._fixture.base
    );
    assert_eq!(
        git_run(&checkout, &["symbolic-ref", "--short", "HEAD"]),
        "develop"
    );
    // Planning did not move the checkout or touch its files.
    assert_eq!(
        fixture.repos[0]._fixture.target(),
        fixture.repos[0]._fixture.base
    );
}

#[test]
fn a_dirty_or_untracked_checked_out_target_is_blocked() {
    for untracked in [false, true] {
        let fixture = Fixture::new();
        let checkout = fixture.repos[0]._fixture.checkout_target();
        if untracked {
            std::fs::write(checkout.join("untracked.txt"), "user file\n").unwrap();
        } else {
            std::fs::write(checkout.join("base.txt"), "user edit\n").unwrap();
        }
        let reason = blocked_reason(prepare_delivery(&fixture.store, &fixture.task).unwrap());
        assert!(reason.contains("uncommitted or untracked"), "{reason}");
        assert!(
            reason.contains("nothing was stashed, reset or checked out"),
            "{reason}"
        );
        assert_eq!(
            fixture.repos[0]._fixture.target(),
            fixture.repos[0]._fixture.base
        );
        assert!(fixture
            .store
            .locks(&fixture.task.task_id)
            .unwrap()
            .is_empty());
        assert!(fixture
            .store
            .delivery_plans(&fixture.task.task_id)
            .unwrap()
            .is_empty());
    }
}

#[test]
fn a_locked_target_worktree_is_blocked() {
    let fixture = Fixture::new();
    let checkout = fixture.repos[0]._fixture.checkout_target();
    git_run(
        &fixture.repos[0]._fixture.root,
        &["worktree", "lock", &checkout.to_string_lossy()],
    );
    let reason = blocked_reason(prepare_delivery(&fixture.store, &fixture.task).unwrap());
    assert!(reason.contains("is locked"), "{reason}");
    assert!(fixture
        .store
        .locks(&fixture.task.task_id)
        .unwrap()
        .is_empty());
}

#[test]
fn a_prepared_plan_is_reused_on_duplicate_and_reopen() {
    let fixture = Fixture::new();
    let first = fixture.plan();
    let second = fixture.plan();
    assert_eq!(first, second);
    assert_eq!(
        fixture.store.locks(&fixture.task.task_id).unwrap(),
        vec![first.repositories[0].lock_key.clone()]
    );

    let reopened = fixture.reopened();
    let task = reopened.get_task(&fixture.task.task_id).unwrap().unwrap();
    match prepare_delivery(&reopened, &task).unwrap() {
        DeliveryPreflight::Planned(plan) => assert_eq!(plan, first),
        other => panic!("expected the persisted plan after reopening, got {other:?}"),
    }
    assert_eq!(
        reopened
            .delivery_plans(&fixture.task.task_id)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        reopened.locks(&fixture.task.task_id).unwrap(),
        vec![first.repositories[0].lock_key.clone()]
    );
}

#[test]
fn a_moved_baseline_requires_reintegration_and_releases_ownership() {
    let fixture = Fixture::new();
    let plan = fixture.plan();
    let moved = fixture.repos[0]
        ._fixture
        .advance_develop("base.txt", "target moved\n");
    assert_ne!(moved, plan.repositories[0].old_oid);

    match prepare_delivery(&fixture.store, &fixture.task).unwrap() {
        DeliveryPreflight::ReintegrationNeeded { reason } => {
            assert!(reason.contains("moved"), "{reason}");
            assert!(reason.contains("not recorded or refreshed"), "{reason}");
        }
        other => panic!("expected reintegration, got {other:?}"),
    }
    // The moved target is not overwritten and the old plan facts remain.
    assert_eq!(fixture.repos[0]._fixture.target(), moved);
    assert_eq!(
        fixture.store.delivery_plans(&fixture.task.task_id).unwrap(),
        vec![plan]
    );
    assert!(fixture
        .store
        .locks(&fixture.task.task_id)
        .unwrap()
        .is_empty());
}

#[test]
fn a_late_target_move_after_ownership_releases_the_attempt_locks() {
    let fixture = Fixture::new();
    let mut moved = None;
    let result = prepare_delivery_observed(&fixture.store, &fixture.task, &mut || {
        moved = Some(
            fixture.repos[0]
                ._fixture
                .advance_develop("base.txt", "late external move\n"),
        );
    })
    .unwrap();
    let moved = moved.expect("the hook ran");
    match result {
        DeliveryPreflight::ReintegrationNeeded { reason } => {
            assert!(reason.contains("moved to"), "{reason}");
            assert!(reason.contains("delivery attempt"), "{reason}");
        }
        other => panic!("expected a late reintegration, got {other:?}"),
    }
    assert_eq!(fixture.repos[0]._fixture.target(), moved);
    assert!(
        fixture
            .store
            .locks(&fixture.task.task_id)
            .unwrap()
            .is_empty(),
        "a failed attempt must not leave its new locks behind"
    );
    assert!(fixture
        .store
        .delivery_plans(&fixture.task.task_id)
        .unwrap()
        .is_empty());
}

#[test]
fn multi_repository_locks_are_assigned_in_canonical_key_order() {
    let fixture = Fixture::with(2, false, None);
    let plan = fixture.plan();
    let mut by_key: Vec<(String, String)> = plan
        .repositories
        .iter()
        .map(|target| (target.lock_key.clone(), target.lock_operation_id.clone()))
        .collect();
    by_key.sort_by(|left, right| left.0.cmp(&right.0));
    for (index, (_, operation)) in by_key.iter().enumerate() {
        assert!(
            operation.ends_with(&format!(".attempt.1.lock.{index}")),
            "{operation} is not the lock for sorted position {index}"
        );
    }
}

#[test]
fn a_persistence_failure_releases_locks_and_retries_with_a_new_attempt() {
    let fixture = Fixture::with(2, false, None);
    let target_before = fixture.repos[0]._fixture.target();
    let unrelated_key = "branch:/unrelated/repo:refs/heads/topic";
    fixture
        .store
        .try_lock("unrelated:lock", unrelated_key, "unrelated-task")
        .unwrap();
    fixture
        .store
        .db()
        .with_conn(|conn| -> rusqlite::Result<()> {
            conn.execute_batch(
                "CREATE TRIGGER fail_delivery_insert BEFORE INSERT ON sa_delivery_plans BEGIN SELECT RAISE(ABORT, 'injected delivery plan failure'); END;",
            )
        })
        .unwrap()
        .unwrap();

    let error = prepare_delivery(&fixture.store, &fixture.task).unwrap_err();
    let text = format!("{error:#}");
    assert!(text.contains("injected delivery plan failure"), "{text}");
    assert!(text.contains("delivery attempt"), "{text}");
    assert!(
        fixture
            .store
            .locks(&fixture.task.task_id)
            .unwrap()
            .is_empty(),
        "{:?}",
        fixture.store.locks(&fixture.task.task_id)
    );
    assert!(fixture
        .store
        .delivery_plans(&fixture.task.task_id)
        .unwrap()
        .is_empty());
    // No Git ref or worktree file changed while the plan could not be saved.
    assert_eq!(fixture.repos[0]._fixture.target(), target_before);
    assert_eq!(
        fixture.repos[0]._fixture.target(),
        fixture.repos[0].entry.baseline_commit
    );
    assert_eq!(
        std::fs::read_to_string(fixture.repos[0]._fixture.root.join("base.txt")).unwrap(),
        "base\n"
    );
    // Ownership recorded by another task is never released.
    assert_eq!(
        fixture.store.locks("unrelated-task").unwrap(),
        vec![unrelated_key.to_owned()]
    );

    fixture
        .store
        .db()
        .with_conn(|conn| -> rusqlite::Result<()> {
            conn.execute_batch("DROP TRIGGER fail_delivery_insert")
        })
        .unwrap()
        .unwrap();

    // The same immutable content retries with a fresh attempt identity.
    let plan = fixture.plan();
    assert!(plan
        .repositories
        .iter()
        .all(|target| target.lock_operation_id.contains(".attempt.2.")));
    assert_eq!(fixture.store.locks(&fixture.task.task_id).unwrap().len(), 2);
    assert_eq!(
        fixture.store.locks("unrelated-task").unwrap(),
        vec![unrelated_key.to_owned()]
    );
    assert_eq!(
        fixture.store.delivery_plans(&fixture.task.task_id).unwrap(),
        vec![plan]
    );
}

#[test]
fn a_competing_task_cannot_take_the_same_branch() {
    let fixture = Fixture::new();
    let plan = fixture.plan();
    let competing = fixture.competing_task();
    let reason = blocked_reason(prepare_delivery(&fixture.store, &competing).unwrap());
    assert!(reason.contains("held by another delivery"), "{reason}");
    assert!(fixture.store.locks(&competing.task_id).unwrap().is_empty());
    assert!(fixture
        .store
        .delivery_plans(&competing.task_id)
        .unwrap()
        .is_empty());
    // The first owner is unaffected.
    assert_eq!(
        fixture.store.locks(&fixture.task.task_id).unwrap(),
        vec![plan.repositories[0].lock_key.clone()]
    );
}

#[test]
fn stale_ownership_release_only_matches_the_owning_task_and_operation() {
    let fixture = Fixture::new();
    let plan = fixture.plan();
    let target = &plan.repositories[0];
    // A different task and operation cannot release the owner's lock.
    assert!(!fixture
        .store
        .unlock(&target.lock_key, "task-other", &target.lock_operation_id)
        .unwrap());
    assert!(!fixture
        .store
        .unlock(&target.lock_key, &plan.task_id, "operation-other")
        .unwrap());
    assert_eq!(
        fixture.store.locks(&fixture.task.task_id).unwrap(),
        vec![target.lock_key.clone()]
    );

    let release = release_delivery(&fixture.store, &plan);
    assert!(release.complete(), "{release:?}");
    assert_eq!(release.released, vec![target.lock_key.clone()]);
    assert!(fixture
        .store
        .locks(&fixture.task.task_id)
        .unwrap()
        .is_empty());

    // Once released, another delivery can own it.
    let competing = fixture.competing_task();
    match prepare_delivery(&fixture.store, &competing).unwrap() {
        DeliveryPreflight::Planned(plan) => {
            assert_eq!(
                fixture.store.locks(&competing.task_id).unwrap(),
                vec![plan.repositories[0].lock_key.clone()]
            );
        }
        other => panic!("expected the competing plan, got {other:?}"),
    }
}

#[test]
fn a_partial_lock_acquisition_releases_only_what_this_attempt_acquired() {
    let fixture = Fixture::with(2, false, None);
    let mut keys: Vec<String> = fixture
        .repos
        .iter()
        .map(|data| {
            branch_lock_key(
                &canonical_common_dir(&data.repo.repo_root).unwrap(),
                "refs/heads/develop",
            )
        })
        .collect();
    keys.sort();
    let blocked_key = keys[1].clone();
    fixture
        .store
        .try_lock("other-task:lock", &blocked_key, "other-task")
        .unwrap();
    // A recorded fact that the failed attempt must not disturb.
    let prepared_events = fixture
        .store
        .events(&fixture.task.task_id)
        .unwrap()
        .iter()
        .filter(|event| event.event_type == EventType::IntegrationPrepared)
        .count();

    let reason = blocked_reason(prepare_delivery(&fixture.store, &fixture.task).unwrap());
    assert!(reason.contains("held by another delivery"), "{reason}");
    assert!(reason.contains("delivery attempt"), "{reason}");
    assert!(
        fixture
            .store
            .locks(&fixture.task.task_id)
            .unwrap()
            .is_empty(),
        "{:?}",
        fixture.store.locks(&fixture.task.task_id)
    );
    assert!(fixture
        .store
        .delivery_plans(&fixture.task.task_id)
        .unwrap()
        .is_empty());
    assert_eq!(
        fixture.store.locks("other-task").unwrap(),
        vec![blocked_key.clone()]
    );
    assert_eq!(
        fixture
            .store
            .events(&fixture.task.task_id)
            .unwrap()
            .iter()
            .filter(|event| event.event_type == EventType::IntegrationPrepared)
            .count(),
        prepared_events
    );
    for data in &fixture.repos {
        assert_eq!(data.repo.integration_status, IntegrationStatus::Prepared);
    }

    // The external owner disappears: the same evidence retries with a fresh
    // durable attempt identity instead of a released operation ID.
    fixture
        .store
        .unlock(&blocked_key, "other-task", "other-task:lock")
        .unwrap();
    let plan = fixture.plan();
    assert_eq!(plan.repositories.len(), 2);
    assert!(
        plan.repositories
            .iter()
            .all(|target| target.lock_operation_id.contains(".attempt.2.")),
        "{:?}",
        plan.repositories
            .iter()
            .map(|target| &target.lock_operation_id)
            .collect::<Vec<_>>()
    );
    assert_eq!(fixture.store.locks(&fixture.task.task_id).unwrap().len(), 2);
    assert_eq!(
        fixture.store.delivery_plans(&fixture.task.task_id).unwrap(),
        vec![plan]
    );
}

#[test]
fn a_foreign_common_dir_is_rejected() {
    let fixture = Fixture::new();
    let mut task = fixture.task.clone();
    task.repositories[0].git_common_dir = fixture.repos[0]._fixture.root.clone();
    let reason = blocked_reason(prepare_delivery(&fixture.store, &task).unwrap());
    assert!(reason.contains("foreign repository"), "{reason}");
}

#[test]
fn duplicate_repository_identities_are_rejected() {
    let fixture = Fixture::new();
    let mut task = fixture.task.clone();
    let duplicate = task.repositories[0].clone();
    task.repositories.push(duplicate);
    let reason = blocked_reason(prepare_delivery(&fixture.store, &task).unwrap());
    assert!(reason.contains("more than once"), "{reason}");
}

#[test]
fn a_changed_tree_requires_a_final_delivery_review() {
    let fixture = Fixture::with(1, true, None);
    let plan = fixture.plan();
    let target = &plan.repositories[0];
    assert_eq!(target.review_stage, ReviewStage::FinalDelivery);
    assert_ne!(target.old_oid, target.new_oid);
    assert_ne!(
        fixture.repos[0]._fixture.tree_of(&target.new_oid),
        fixture.repos[0]._fixture.tree_of(&target.reviewed_commit)
    );

    // An implementation-stage decision cannot approve the changed tree.
    let stale = Fixture::with(
        1,
        true,
        Some((ReviewStage::Implementation, ReviewVerdict::Approved)),
    );
    let reason = blocked_reason(prepare_delivery(&stale.store, &stale.task).unwrap());
    assert!(
        reason.contains("no host-decided FINAL_DELIVERY review"),
        "{reason}"
    );
}

#[test]
fn same_tree_reuse_uses_the_implementation_review() {
    let fixture = Fixture::new();
    let plan = fixture.plan();
    assert_eq!(
        plan.repositories[0].review_stage,
        ReviewStage::Implementation
    );
    assert_eq!(
        fixture.repos[0].entry.integrated_tree,
        fixture.repos[0]
            ._fixture
            .tree_of(&fixture.repos[0].entry.reviewed_commit)
    );

    // A final-delivery-only decision does not satisfy same-tree reuse.
    let stale = Fixture::with(
        1,
        false,
        Some((ReviewStage::FinalDelivery, ReviewVerdict::Approved)),
    );
    let reason = blocked_reason(prepare_delivery(&stale.store, &stale.task).unwrap());
    assert!(
        reason.contains("no host-decided IMPLEMENTATION review"),
        "{reason}"
    );
}

#[test]
fn an_unapproved_decision_blocks_delivery() {
    let fixture = Fixture::with(
        1,
        false,
        Some((ReviewStage::Implementation, ReviewVerdict::ChangesRequested)),
    );
    let reason = blocked_reason(prepare_delivery(&fixture.store, &fixture.task).unwrap());
    assert!(reason.contains("did not approve delivery"), "{reason}");
    assert!(fixture
        .store
        .locks(&fixture.task.task_id)
        .unwrap()
        .is_empty());
}

#[test]
fn two_repositories_one_moved_target_requires_reintegration() {
    let fixture = Fixture::with(2, false, None);
    let plan = fixture.plan();
    assert_eq!(plan.repositories.len(), 2);
    // Test code moves the target directly; the cooperative lock does not and
    // cannot stop an external Git command.
    let moved = fixture.repos[0]
        ._fixture
        .advance_develop("base.txt", "external move\n");
    match prepare_delivery(&fixture.store, &fixture.task).unwrap() {
        DeliveryPreflight::ReintegrationNeeded { reason } => {
            assert!(reason.contains("repo-1"), "{reason}");
        }
        other => panic!("expected reintegration, got {other:?}"),
    }
    assert_eq!(fixture.repos[0]._fixture.target(), moved);
    assert_eq!(
        fixture.repos[1]._fixture.target(),
        plan.repositories[1].old_oid
    );
    assert!(fixture
        .store
        .locks(&fixture.task.task_id)
        .unwrap()
        .is_empty());
    assert_eq!(
        fixture.store.delivery_plans(&fixture.task.task_id).unwrap(),
        vec![plan]
    );
}

#[test]
fn a_claim_never_overrides_the_actual_target_oid() {
    let fixture = Fixture::new();
    let plan = fixture.plan();
    let target = &plan.repositories[0];
    // The Coder's own report claims a successful delivery; it is not evidence.
    fixture
        .store
        .add_report(
            "agent-delivery-claim",
            &fixture.task.task_id,
            ReportKind::Integration,
            fixture.task.spec_version,
            fixture.task.review_round,
            json!({"spec_version": 1, "round": 0, "targets": [{
                "repository_id": target.repository_id,
                "target_branch": target.target_branch,
                "before_commit": target.old_oid,
                "after_commit": target.new_oid,
                "conflict": false,
                "tests_passed": true,
                "issues": []
            }]}),
        )
        .unwrap();
    let elsewhere = fixture.repos[0]._fixture.move_target_elsewhere();
    assert_ne!(elsewhere, target.new_oid);

    let verification = verify_delivery(&fixture.task, &plan).unwrap();
    assert!(!verification.complete);
    assert!(!verification.repositories[0].verified);
    assert_eq!(
        verification.repositories[0].actual_commit.as_deref(),
        Some(elsewhere.as_str())
    );
    assert!(
        verification.failures[0].contains("target branch is at"),
        "{verification:?}"
    );

    // Only the real target object makes verification pass.
    fixture.repos[0]._fixture.set_target(&target.new_oid);
    let verification = verify_delivery(&fixture.task, &plan).unwrap();
    assert!(verification.complete, "{verification:?}");
    assert!(verification.pending.is_empty());
    assert_eq!(
        verification.repositories[0].actual_commit.as_deref(),
        Some(target.new_oid.as_str())
    );
}

#[test]
fn a_partial_delivery_verification_preserves_verified_repositories() {
    let fixture = Fixture::with(2, false, None);
    let plan = fixture.plan();
    fixture.repos[0]
        ._fixture
        .set_target(&plan.repositories[0].new_oid);
    let verification = verify_delivery(&fixture.task, &plan).unwrap();
    assert!(!verification.complete);
    assert!(verification.repositories[0].verified);
    assert!(!verification.repositories[1].verified);
    assert_eq!(
        verification.pending,
        vec![plan.repositories[1].repository_id.clone()]
    );
    assert_eq!(verification.failures.len(), 1);
    assert_eq!(
        verification.repositories[0].actual_commit.as_deref(),
        Some(plan.repositories[0].new_oid.as_str())
    );

    fixture.repos[1]
        ._fixture
        .set_target(&plan.repositories[1].new_oid);
    let verification = verify_delivery(&fixture.task, &plan).unwrap();
    assert!(verification.complete, "{verification:?}");
    assert!(verification.pending.is_empty());
    assert!(verification.repositories.iter().all(|repo| repo.verified));
}

#[test]
fn a_new_dirty_checkout_cannot_verify_a_ref_only_plan() {
    // The parent counterexample: a RefOnly plan, then a linked checkout of the
    // target, then an external update-ref to the approved OID. The checkout's
    // index and files remain old, so it is not proof of delivery.
    let fixture = Fixture::new();
    let plan = fixture.plan();
    let target = &plan.repositories[0];
    assert_eq!(target.destination, DeliveryDestination::RefOnly);

    let checkout = fixture.repos[0]._fixture.checkout_target();
    fixture.repos[0]._fixture.set_target(&target.new_oid);
    let status = git_run(&checkout, &["status", "--porcelain"]);
    assert!(status.contains("D  reviewed.txt"), "{status}");
    assert_eq!(
        git_run(&checkout, &["rev-parse", "HEAD"]),
        target.new_oid,
        "the ref moved but the working tree did not"
    );

    let verification = verify_delivery(&fixture.task, &plan).unwrap();
    assert!(!verification.complete, "{verification:?}");
    assert!(!verification.repositories[0].verified);
    assert!(
        verification.repositories[0]
            .reason
            .contains("uncommitted or untracked changes"),
        "{verification:?}"
    );
    assert_eq!(
        verification.pending,
        vec![target.repository_id.clone()],
        "{verification:?}"
    );
}

#[test]
fn a_new_untracked_checkout_cannot_verify_a_ref_only_plan() {
    let fixture = Fixture::new();
    let plan = fixture.plan();
    let target = &plan.repositories[0];
    fixture.repos[0]._fixture.set_target(&target.new_oid);
    let checkout = fixture.repos[0]._fixture.checkout_target();
    std::fs::write(checkout.join("user-note.txt"), "not delivered\n").unwrap();

    let verification = verify_delivery(&fixture.task, &plan).unwrap();
    assert!(!verification.complete, "{verification:?}");
    assert!(
        verification.repositories[0]
            .reason
            .contains("uncommitted or untracked changes"),
        "{verification:?}"
    );
    assert!(
        verification.repositories[0]
            .reason
            .contains("user-note.txt"),
        "{verification:?}"
    );
}

#[test]
fn a_clean_new_checkout_can_verify_a_ref_only_plan() {
    let fixture = Fixture::new();
    let plan = fixture.plan();
    let target = &plan.repositories[0];
    // Moving the ref first means the new checkout is created from the approved
    // commit, so current host facts prove a clean delivered working tree.
    fixture.repos[0]._fixture.set_target(&target.new_oid);
    let checkout = fixture.repos[0]._fixture.checkout_target();
    assert_eq!(git_run(&checkout, &["status", "--porcelain"]), "");
    assert_eq!(git_run(&checkout, &["rev-parse", "HEAD"]), target.new_oid);

    let verification = verify_delivery(&fixture.task, &plan).unwrap();
    assert!(verification.complete, "{verification:?}");
    assert!(verification.repositories[0].verified);
    assert!(verification.pending.is_empty());
}

#[test]
fn a_ref_only_target_still_verifies_when_really_unchecked_out() {
    let fixture = Fixture::new();
    let plan = fixture.plan();
    let checkouts = worktree_entries(&fixture.repos[0]._fixture.root)
        .unwrap()
        .into_iter()
        .filter(|entry| entry.branch.as_deref() == Some("refs/heads/develop"))
        .count();
    assert_eq!(checkouts, 0);

    fixture.repos[0]
        ._fixture
        .set_target(&plan.repositories[0].new_oid);
    let verification = verify_delivery(&fixture.task, &plan).unwrap();
    assert!(verification.complete, "{verification:?}");
}

#[test]
fn a_locked_checkout_cannot_be_verified() {
    let fixture = Fixture::new();
    let plan = fixture.plan();
    fixture.repos[0]
        ._fixture
        .set_target(&plan.repositories[0].new_oid);
    let checkout = fixture.repos[0]._fixture.checkout_target();
    git_run(
        &fixture.repos[0]._fixture.root,
        &["worktree", "lock", &checkout.to_string_lossy()],
    );

    let verification = verify_delivery(&fixture.task, &plan).unwrap();
    assert!(!verification.complete, "{verification:?}");
    assert!(
        verification.repositories[0].reason.contains("is locked"),
        "{verification:?}"
    );
}

#[test]
fn a_duplicate_target_checkout_is_ambiguous() {
    let fixture = Fixture::new();
    let plan = fixture.plan();
    fixture.repos[0]
        ._fixture
        .set_target(&plan.repositories[0].new_oid);
    let first = fixture.repos[0]._fixture.checkout_target();
    let second = fixture.repos[0]
        ._fixture
        .common
        .join("second target checkout");
    // Git normally refuses a second checkout of the same branch; the fixture
    // forces it to model an ambiguous external topology.
    git_run(
        &fixture.repos[0]._fixture.root,
        &[
            "worktree",
            "add",
            "--force",
            &second.to_string_lossy(),
            "develop",
        ],
    );
    assert!(first.is_dir() && second.is_dir());

    let verification = verify_delivery(&fixture.task, &plan).unwrap();
    assert!(!verification.complete, "{verification:?}");
    assert!(
        verification.repositories[0].reason.contains("ambiguous"),
        "{verification:?}"
    );
}

#[test]
fn a_missing_planned_checkout_is_blocked() {
    let fixture = Fixture::new();
    let checkout = fixture.repos[0]._fixture.checkout_target();
    let plan = fixture.plan();
    assert!(matches!(
        plan.repositories[0].destination,
        DeliveryDestination::CheckedOut { .. }
    ));
    fixture.repos[0]
        ._fixture
        .set_target(&plan.repositories[0].new_oid);
    git_run(
        &fixture.repos[0]._fixture.root,
        &["worktree", "remove", "--force", &checkout.to_string_lossy()],
    );

    let verification = verify_delivery(&fixture.task, &plan).unwrap();
    assert!(!verification.complete, "{verification:?}");
    assert!(
        verification.repositories[0]
            .reason
            .contains("planned target checkout"),
        "{verification:?}"
    );
}

#[test]
fn a_moved_planned_checkout_is_blocked() {
    let fixture = Fixture::new();
    let checkout = fixture.repos[0]._fixture.checkout_target();
    let plan = fixture.plan();
    fixture.repos[0]
        ._fixture
        .set_target(&plan.repositories[0].new_oid);
    let moved = fixture.repos[0]._fixture.common.join("moved checkout");
    git_run(
        &fixture.repos[0]._fixture.root,
        &[
            "worktree",
            "move",
            &checkout.to_string_lossy(),
            &moved.to_string_lossy(),
        ],
    );

    let verification = verify_delivery(&fixture.task, &plan).unwrap();
    assert!(!verification.complete, "{verification:?}");
    assert!(
        verification.repositories[0]
            .reason
            .contains("moved or was replaced"),
        "{verification:?}"
    );
}

#[test]
fn a_switched_planned_checkout_is_blocked() {
    let fixture = Fixture::new();
    let checkout = fixture.repos[0]._fixture.checkout_target();
    let plan = fixture.plan();
    fixture.repos[0]
        ._fixture
        .set_target(&plan.repositories[0].new_oid);
    // The planned checkout changes branch, so the target ref is no longer
    // represented by a working tree at that path.
    git_run(&checkout, &["switch", "-q", "-c", "other"]);

    let verification = verify_delivery(&fixture.task, &plan).unwrap();
    assert!(!verification.complete, "{verification:?}");
    assert!(
        verification.repositories[0]
            .reason
            .contains("no longer on develop"),
        "{verification:?}"
    );
}

#[cfg(unix)]
#[test]
fn a_foreign_checkout_is_rejected() {
    let fixture = Fixture::new();
    let plan = fixture.plan();
    fixture.repos[0]
        ._fixture
        .set_target(&plan.repositories[0].new_oid);
    let checkout = fixture.repos[0]._fixture.checkout_target();
    let other = RepoFixture::new("other");
    std::fs::remove_dir_all(&checkout).unwrap();
    std::os::unix::fs::symlink(&other.root, &checkout).unwrap();

    let verification = verify_delivery(&fixture.task, &plan).unwrap();
    assert!(!verification.complete, "{verification:?}");
    assert!(
        verification.repositories[0].reason.contains("belongs to"),
        "{verification:?}"
    );
}

#[test]
fn two_repositories_one_checkout_failure_keeps_the_other_verified() {
    let fixture = Fixture::with(2, false, None);
    let plan = fixture.plan();
    let stale_checkout = fixture.repos[1]._fixture.checkout_target();
    fixture.repos[1]
        ._fixture
        .set_target(&plan.repositories[1].new_oid);
    assert!(git_run(&stale_checkout, &["status", "--porcelain"]).contains("D  reviewed.txt"));
    fixture.repos[0]
        ._fixture
        .set_target(&plan.repositories[0].new_oid);

    let verification = verify_delivery(&fixture.task, &plan).unwrap();
    assert!(!verification.complete, "{verification:?}");
    assert!(verification.repositories[0].verified);
    assert!(!verification.repositories[1].verified);
    assert_eq!(
        verification.pending,
        vec![plan.repositories[1].repository_id.clone()]
    );
    assert_eq!(
        verification.repositories[0].actual_commit.as_deref(),
        Some(plan.repositories[0].new_oid.as_str())
    );
}

#[test]
fn a_stale_plan_cannot_verify_delivery() {
    let fixture = Fixture::with(2, false, None);
    let plan = fixture.plan();
    for (data, target) in fixture.repos.iter().zip(plan.repositories.iter()) {
        data._fixture.set_target(&target.new_oid);
    }
    assert!(verify_delivery(&fixture.task, &plan).unwrap().complete);

    let mut later_spec = fixture.task.clone();
    later_spec.spec_version += 1;
    assert!(verify_delivery(&later_spec, &plan).is_err());

    let mut later_review = fixture.task.clone();
    later_review.review_round += 1;
    assert!(verify_delivery(&later_review, &plan).is_err());

    // A task whose repository set no longer matches the plan cannot verify
    // only the overlapping repositories.
    let mut fewer = fixture.task.clone();
    fewer.repositories.pop();
    assert!(verify_delivery(&fewer, &plan).is_err());

    let mut extra = fixture.task.clone();
    let mut extra_repo = fixture.repos[0].repo.clone();
    extra_repo.repository_id = "repo-extra".into();
    extra.repositories.push(extra_repo);
    assert!(verify_delivery(&extra, &plan).is_err());

    let mut other_task = fixture.task.clone();
    other_task.task_id = "task-other".into();
    assert!(verify_delivery(&other_task, &plan).is_err());
}

#[test]
fn a_non_git_only_task_has_no_target_branch_delivery() {
    let fixture = Fixture::new();
    let mut task = fixture.task.clone();
    task.repositories.clear();
    task.non_git_targets = vec![NonGitTarget {
        task_id: task.task_id.clone(),
        path: fixture._temp.path().join("notes"),
        snapshot_dir: fixture._temp.path().join("snapshot"),
        snapshot_manifest_json: "{}".into(),
        diff_ref: Some("snapshot-1".into()),
        status: NonGitStatus::Prepared,
        lock_key: format!("path:{}", fixture._temp.path().join("notes").display()),
        submission: None,
    }];
    assert_eq!(
        prepare_delivery(&fixture.store, &task).unwrap(),
        DeliveryPreflight::NotApplicable
    );
    assert!(fixture.store.locks(&task.task_id).unwrap().is_empty());
    assert!(fixture
        .store
        .delivery_plans(&task.task_id)
        .unwrap()
        .is_empty());
}

fn run_command(cwd: &Path, command: &str) -> std::process::Output {
    Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap()
}

/// A minimal target plan for command-level tests. Only the fields the command
/// builder reads are meaningful.
fn command_target(
    root: &Path,
    common: &Path,
    old_oid: &str,
    new_oid: &str,
    destination: DeliveryDestination,
) -> DeliveryTargetPlan {
    DeliveryTargetPlan {
        repository_id: "repo-1".into(),
        repo_root: root.to_path_buf(),
        git_common_dir: common.to_path_buf(),
        target_branch: "develop".into(),
        target_ref: "refs/heads/develop".into(),
        destination,
        baseline_commit: old_oid.into(),
        reviewed_commit: old_oid.into(),
        integrated_commit: new_oid.into(),
        integrated_tree: String::new(),
        old_oid: old_oid.into(),
        new_oid: new_oid.into(),
        review_stage: ReviewStage::Implementation,
        lock_key: "branch:test:refs/heads/develop".into(),
        lock_operation_id: "lock".into(),
    }
}

/// A repository whose target branch is checked out in a linked worktree with a
/// space in its path, plus an integrated commit on `main` that adds a file the
/// baseline ignores. Returns the paths and object IDs.
fn ignored_collision_repo() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf, String, String) {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("repo");
    std::fs::create_dir(&root).unwrap();
    git_run(&root, &["init", "-q", "-b", "main"]);
    git_run(&root, &["config", "user.email", "test@example.invalid"]);
    git_run(&root, &["config", "user.name", "Test"]);
    git_run(&root, &["config", "commit.gpgsign", "false"]);
    std::fs::write(root.join(".gitignore"), "secret.txt\n").unwrap();
    std::fs::write(root.join("base.txt"), "base\n").unwrap();
    git_run(&root, &["add", "."]);
    git_run(&root, &["commit", "-q", "-m", "base"]);
    git_run(&root, &["branch", "develop"]);
    let root = root.canonicalize().unwrap();
    let common = PathBuf::from(git_run(
        &root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    ))
    .canonicalize()
    .unwrap();
    let checkout = common.join("target checkout");
    git_run(
        &root,
        &[
            "worktree",
            "add",
            "-q",
            &checkout.to_string_lossy(),
            "develop",
        ],
    );
    let old = git_run(&root, &["rev-parse", "refs/heads/develop"]);
    std::fs::write(checkout.join("secret.txt"), "user ignored bytes\n").unwrap();
    // The integrated commit adds the same ignored path.
    std::fs::write(root.join("secret.txt"), "delivered\n").unwrap();
    git_run(&root, &["add", "-f", "secret.txt"]);
    git_run(&root, &["commit", "-q", "-m", "add ignored file"]);
    let new = git_run(&root, &["rev-parse", "HEAD"]);
    (temp, root, checkout, common, old, new)
}

#[test]
fn a_ref_only_delivery_command_is_a_compare_and_swap() {
    let fixture = Fixture::new();
    let plan = fixture.plan();
    let target = &plan.repositories[0];
    assert_eq!(target.destination, DeliveryDestination::RefOnly);
    let command = delivery_command(target);
    assert!(
        command.contains(&format!(
            "update-ref 'refs/heads/develop' {} {}",
            target.new_oid, target.old_oid
        )),
        "{command}"
    );

    let output = run_command(&fixture.repos[0]._fixture.root, &command);
    assert!(
        output.status.success(),
        "{command}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(fixture.repos[0]._fixture.target(), target.new_oid);
    assert!(
        !fixture.repos[0]._fixture.root.join("reviewed.txt").exists(),
        "update-ref must not touch a working tree"
    );

    // The captured old OID is the CAS expectation: a second run must fail and
    // must not move the reference again.
    let moved = fixture.repos[0]._fixture.move_target_elsewhere();
    let output = run_command(&fixture.repos[0]._fixture.root, &command);
    assert!(!output.status.success(), "{command} unexpectedly succeeded");
    assert_eq!(fixture.repos[0]._fixture.target(), moved);
}

#[test]
fn a_checked_out_delivery_command_fast_forwards_a_real_checkout() {
    let fixture = Fixture::new();
    let checkout = fixture.repos[0]._fixture.checkout_target();
    assert!(checkout.to_string_lossy().contains(' '));
    let plan = fixture.plan();
    let target = &plan.repositories[0];
    assert_eq!(
        target.destination,
        DeliveryDestination::CheckedOut {
            path: checkout.clone()
        }
    );
    let command = delivery_command(target);
    assert!(
        command.contains("merge --ff-only --no-overwrite-ignore"),
        "{command}"
    );

    let output = run_command(&fixture.repos[0]._fixture.root, &command);
    assert!(
        output.status.success(),
        "{command}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(git_run(&checkout, &["rev-parse", "HEAD"]), target.new_oid);
    assert_eq!(fixture.repos[0]._fixture.target(), target.new_oid);
    assert_eq!(git_run(&checkout, &["status", "--porcelain"]), "");
    assert_eq!(
        std::fs::read_to_string(checkout.join("reviewed.txt")).unwrap(),
        "value=1\n"
    );
    // A clean checkout at the approved commit is exactly what verification
    // treats as delivered.
    assert!(verify_delivery(&fixture.task, &plan).unwrap().complete);
}

#[test]
fn an_ignored_file_collision_aborts_the_checked_out_delivery() {
    let (_temp, root, checkout, common, old, new) = ignored_collision_repo();
    let target = command_target(
        &root,
        &common,
        &old,
        &new,
        DeliveryDestination::CheckedOut {
            path: checkout.clone(),
        },
    );
    let command = delivery_command(&target);
    let output = run_command(&root, &command);
    assert!(
        !output.status.success(),
        "the ignored collision must abort: {command}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("would be overwritten") || stderr.contains("untracked working tree"),
        "{stderr}"
    );
    // The user's ignored bytes, the checkout HEAD and the branch are unchanged.
    assert_eq!(
        std::fs::read_to_string(checkout.join("secret.txt")).unwrap(),
        "user ignored bytes\n"
    );
    assert_eq!(git_run(&checkout, &["rev-parse", "HEAD"]), old);
    assert_eq!(git_run(&root, &["rev-parse", "refs/heads/develop"]), old);
}

#[test]
fn the_checked_out_delivery_command_refuses_a_switched_checkout() {
    let fixture = Fixture::new();
    let checkout = fixture.repos[0]._fixture.checkout_target();
    let plan = fixture.plan();
    let target = &plan.repositories[0];
    let command = delivery_command(target);
    // An external process switches the planned checkout to another branch.
    git_run(&checkout, &["switch", "-q", "-c", "elsewhere"]);
    let other_before = git_run(&checkout, &["rev-parse", "HEAD"]);

    let output = run_command(&fixture.repos[0]._fixture.root, &command);
    assert!(
        !output.status.success(),
        "the guarded command must refuse a switched checkout: {command}"
    );
    assert_eq!(git_run(&checkout, &["rev-parse", "HEAD"]), other_before);
    assert_eq!(
        fixture.repos[0]._fixture.target(),
        fixture.repos[0].entry.baseline_commit
    );
}

#[test]
fn a_delivery_dispatch_is_recorded_once_and_rejects_conflicting_facts() {
    let fixture = Fixture::new();
    let plan = fixture.plan();
    let dispatch = DeliveryDispatch {
        task_id: fixture.task.task_id.clone(),
        spec_version: plan.spec_version,
        review_round: plan.review_round,
        plan_operation_id: plan.operation_id.clone(),
        coder_session_id: "coder-session".into(),
        purpose: "delivery".into(),
        job_id: None,
        created_at: "2026-01-01T00:00:00Z".into(),
    };
    let stored = fixture.store.begin_delivery_dispatch(&dispatch).unwrap();
    assert_eq!(stored, dispatch);
    // Replay returns the same receipt instead of a second dispatch.
    assert_eq!(
        fixture.store.begin_delivery_dispatch(&dispatch).unwrap(),
        dispatch
    );
    let bound = fixture
        .store
        .set_delivery_dispatch_job(&dispatch.task_id, &plan.operation_id, "job-1")
        .unwrap();
    assert_eq!(bound.job_id.as_deref(), Some("job-1"));
    // The same job is idempotent; another job or session is a conflict.
    assert_eq!(
        fixture
            .store
            .set_delivery_dispatch_job(&dispatch.task_id, &plan.operation_id, "job-1")
            .unwrap()
            .job_id
            .as_deref(),
        Some("job-1")
    );
    assert!(fixture
        .store
        .set_delivery_dispatch_job(&dispatch.task_id, &plan.operation_id, "job-2")
        .is_err());
    let mut other_session = dispatch.clone();
    other_session.coder_session_id = "another-coder".into();
    assert!(fixture
        .store
        .begin_delivery_dispatch(&other_session)
        .is_err());
    assert_eq!(
        fixture
            .store
            .delivery_dispatch(&dispatch.task_id, &plan.operation_id)
            .unwrap()
            .unwrap()
            .job_id
            .as_deref(),
        Some("job-1")
    );
}

#[test]
fn recording_a_delivery_marks_the_repository_and_is_idempotent() {
    let fixture = Fixture::new();
    let plan = fixture.plan();
    // The delivery observation is only valid once the host has reached the
    // delivery phase; the surrounding orchestrator produces that state.
    fixture
        .store
        .db()
        .with_conn(|conn| -> rusqlite::Result<usize> {
            conn.execute(
                "UPDATE sa_tasks SET phase='DELIVERY_PENDING', status='RUNNING' WHERE task_id=?1",
                [&fixture.task.task_id],
            )
        })
        .unwrap()
        .unwrap();
    let operation = "orchestrator:delivery-completed:test";
    let payload = json!({"complete": true});
    let first = fixture
        .store
        .record_delivery_completed(&plan, operation, payload.clone())
        .unwrap();
    assert!(first.inserted);
    let replay = fixture
        .store
        .record_delivery_completed(&plan, operation, payload)
        .unwrap();
    assert!(!replay.inserted);
    assert_eq!(replay.event.event_id, first.event.event_id);
    let repo = fixture
        .store
        .get_task(&fixture.task.task_id)
        .unwrap()
        .unwrap()
        .repositories
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(repo.integration_status, IntegrationStatus::Completed);

    // A different operation for the same spec/round returns the one recorded
    // delivery observation instead of appending a second fact.
    let other = fixture
        .store
        .record_delivery_completed(&plan, "orchestrator:other", json!({"complete": true}))
        .unwrap();
    assert!(!other.inserted);
    assert_eq!(other.event.event_id, first.event.event_id);
    // An incomplete payload is rejected outright.
    assert!(fixture
        .store
        .record_delivery_completed(&plan, "orchestrator:incomplete", json!({"complete": false}))
        .is_err());
}

#[test]
fn a_released_plan_is_not_owned_and_names_the_actual_holder() {
    let fixture = Fixture::new();
    let plan = fixture.plan();
    assert!(delivery_ownership(&fixture.store, &plan)
        .unwrap()
        .complete());
    let release = release_delivery(&fixture.store, &plan);
    assert!(release.complete(), "{release:?}");

    let released = delivery_ownership(&fixture.store, &plan).unwrap();
    assert!(!released.complete());
    assert!(
        released.reason().unwrap().contains("is not owned"),
        "{released:?}"
    );

    fixture
        .store
        .try_lock("other:lock", &plan.repositories[0].lock_key, "other-task")
        .unwrap();
    let held = delivery_ownership(&fixture.store, &plan).unwrap();
    assert!(!held.complete());
    let reason = held.reason().unwrap();
    assert!(reason.contains("other-task"), "{reason}");
    // A loaded plan never reuses another task's ownership.
    assert!(!held.repositories[0].owned);
}

#[test]
fn a_plan_must_still_bind_the_current_host_evidence() {
    let fixture = Fixture::new();
    let plan = fixture.plan();
    assert_eq!(
        verify_plan_binding(&fixture.store, &fixture.task, &plan).unwrap(),
        None
    );

    let mut later_spec = fixture.task.clone();
    later_spec.spec_version += 1;
    assert!(verify_plan_binding(&fixture.store, &later_spec, &plan)
        .unwrap()
        .is_some());

    let mut later_review = fixture.task.clone();
    later_review.review_round += 1;
    assert!(verify_plan_binding(&fixture.store, &later_review, &plan)
        .unwrap()
        .is_some());

    let mut changed = fixture.task.clone();
    changed.repositories[0].integrated_commit = Some("0".repeat(40));
    assert!(verify_plan_binding(&fixture.store, &changed, &plan)
        .unwrap()
        .is_some());
}

/// Runs a delivery command with a `git` wrapper that fails the listed read
/// subcommands and records any write subcommand that would be executed. The
/// wrapper delegates everything else to the real Git.
#[cfg(unix)]
fn run_with_failing_read(
    cwd: &Path,
    command: &str,
    failing: &[&str],
) -> (std::process::Output, Vec<String>) {
    use std::os::unix::fs::PermissionsExt;
    let real = std::process::Command::new("sh")
        .arg("-c")
        .arg("command -v git")
        .output()
        .unwrap();
    let real = String::from_utf8_lossy(&real.stdout).trim().to_owned();
    assert!(!real.is_empty(), "no git executable");
    let temp = tempfile::tempdir().unwrap();
    let bin = temp.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let log = temp.path().join("writes.log");
    let script = format!(
        "#!/bin/sh\nfor arg in \"$@\"; do\n  case \"$arg\" in\n    {patterns}) echo 'fatal: injected read failure' >&2; exit 128;;\n  esac\n  case \"$arg\" in\n    update-ref|merge) printf '%s\\n' \"$arg\" >> {log};;\n  esac\ndone\nexec {real} \"$@\"\n",
        patterns = failing.join("|"),
        log = shell_quote(&log.to_string_lossy()),
        real = shell_quote(&real)
    );
    let wrapper = bin.join("git");
    std::fs::write(&wrapper, script).unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .env("PATH", path)
        .env("LC_ALL", "C")
        .output()
        .unwrap();
    let writes = std::fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect();
    (output, writes)
}

#[cfg(unix)]
#[test]
fn a_failed_worktree_inspection_never_updates_the_ref() {
    let fixture = Fixture::new();
    let plan = fixture.plan();
    let target = &plan.repositories[0];
    assert_eq!(target.destination, DeliveryDestination::RefOnly);
    let (output, writes) = run_with_failing_read(
        &fixture.repos[0]._fixture.root,
        &delivery_command(target),
        &["worktree"],
    );
    assert!(
        !output.status.success(),
        "a failed worktree listing must abort: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        writes.is_empty(),
        "the guard still attempted a write: {writes:?}"
    );
    assert_eq!(
        fixture.repos[0]._fixture.target(),
        target.old_oid,
        "the target moved despite a failed inspection"
    );
}

#[cfg(unix)]
#[test]
fn a_failed_status_inspection_never_fast_forwards() {
    let fixture = Fixture::new();
    let checkout = fixture.repos[0]._fixture.checkout_target();
    let plan = fixture.plan();
    let target = &plan.repositories[0];
    let (output, writes) = run_with_failing_read(&checkout, &delivery_command(target), &["status"]);
    assert!(
        !output.status.success(),
        "a failed status read must abort: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        writes.is_empty(),
        "the guard still attempted a write: {writes:?}"
    );
    assert_eq!(git_run(&checkout, &["rev-parse", "HEAD"]), target.old_oid);
    assert_eq!(fixture.repos[0]._fixture.target(), target.old_oid);
}
