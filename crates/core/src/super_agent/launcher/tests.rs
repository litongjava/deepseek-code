//! Deterministic crash-window tests for durable child launch and resume.
//!
//! Each gate simulates a host crash after the preceding durable step. A new
//! service context and launcher then reopen the same data directory, exactly as
//! the next tick would, and must converge without creating a second session,
//! job or instruction. The supervisor is a real local subprocess (a small
//! shell script) that records every launch, so process counts are observed
//! rather than inferred from in-memory state.
#![cfg(unix)]

use super::*;
use crate::runtime::{RuntimePaths, SessionStore};
use crate::services::background_sessions;
use crate::services::EnvironmentCredentials;
use crate::super_agent::config::SuperAgentConfig;
use crate::super_agent::role::{self as roles, Role};
use crate::super_agent::store::TaskStore;
use crate::super_agent::{IntegrationStatus, RepoTarget, TaskRecord, TaskSpec, VerificationStatus};
use serde_json::json;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
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

struct Fixture {
    _temp: tempfile::TempDir,
    project: PathBuf,
    data: PathBuf,
    context: ServiceContext,
    store: TaskStore,
    router: String,
    task_id: String,
    supervisor: PathBuf,
    launch_log: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir(&project).unwrap();
        git(&project, &["init", "-q", "-b", "main"]);
        git(&project, &["config", "user.email", "test@example.invalid"]);
        git(&project, &["config", "user.name", "Test"]);
        std::fs::write(project.join("file.txt"), "initial\n").unwrap();
        git(&project, &["add", "."]);
        git(&project, &["commit", "-q", "-m", "initial"]);
        let project = project.canonicalize().unwrap();
        let data = temp.path().join("data");
        let context = ServiceContext::open(
            RuntimePaths::new(data.clone()).unwrap(),
            Arc::new(EnvironmentCredentials),
        )
        .unwrap();
        let router = SessionStore::new(context.db.clone())
            .create(&project, "fixture", "fixture")
            .unwrap();
        roles::set(&context.db, &router, Role::Router).unwrap();
        let store = TaskStore::new(context.db.clone(), data.clone());
        let spec = TaskSpec {
            title: "Launcher fixture".into(),
            workspace: project.clone(),
            targets: vec![project.to_string_lossy().into_owned()],
            goal: "Exercise durable launch recovery".into(),
            requirements: Vec::new(),
            acceptance_criteria: Vec::new(),
            constraints: Vec::new(),
            out_of_scope: Vec::new(),
            target_branch: Some("main".into()),
            repository_target_branches: Default::default(),
        };
        let task = store.create_task(&router, spec, "create").unwrap();
        let head = git(&project, &["rev-parse", "HEAD"]);
        store
            .save_repo(&RepoTarget {
                task_id: task.task_id.clone(),
                repository_id: "repo-1".into(),
                repo_root: project.clone(),
                git_common_dir: project.join(".git"),
                target_path: project.clone(),
                target_branch: "main".into(),
                task_branch: "agent/task".into(),
                worktree_path: project.clone(),
                working_path: project.clone(),
                base_commit: head,
                submitted_commit: None,
                reviewed_commit: None,
                integrated_commit: None,
                integration_status: IntegrationStatus::Pending,
                verification_status: VerificationStatus::Pending,
                source_state: Vec::new(),
            })
            .unwrap();
        let launch_log = temp.path().join("child-launches.log");
        // The same executable plays both roles, exactly like `dcl`: with the
        // worker variable set it is the real production supervisor entry; as
        // the launched child it records one invocation and exits.
        let supervisor = temp.path().join("dcl-fixture.sh");
        let test_binary = std::env::current_exe().unwrap();
        std::fs::write(
            &supervisor,
            format!(
                "#!/bin/sh\nif [ -n \"$DEEPSEEK_CODE_BACKGROUND_JOB\" ]; then\n  exec '{}' --exact 'services::background_sessions::tests::run_worker_entry' --ignored --nocapture\nfi\necho run >> '{}'\nexit 0\n",
                test_binary.display(),
                launch_log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&supervisor, std::fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            _temp: temp,
            project,
            data,
            context,
            store,
            router,
            task_id: task.task_id,
            supervisor,
            launch_log,
        }
    }

    fn task(&self) -> TaskRecord {
        self.store.get_task(&self.task_id).unwrap().unwrap()
    }

    fn provider(&self) -> ResolvedProvider {
        let mut provider: ResolvedProvider = ResolvedProvider {
            provider: serde_json::from_value(json!({
                "id":"fixture","name":"Fixture","base_url":"http://127.0.0.1:1",
                "api_key":"fixture-key","api_format":"openai","models":[],"enabled":true
            }))
            .unwrap(),
            model: serde_json::from_value(json!({
                "id":"router-model","name":"router-model","enabled":true,
                "supports_vision":false,"supports_web_search":false
            }))
            .unwrap(),
        };
        provider.provider.models = vec![provider.model.clone()];
        provider
    }

    fn config() -> SuperAgentConfig {
        crate::super_agent::config::parse(&json!({"agents":{}})).unwrap()
    }

    fn launcher(&self) -> Launcher {
        self.launcher_with_gate(None)
    }

    fn launcher_with_gate(&self, gate: Option<LaunchGate>) -> Launcher {
        Launcher {
            context: self.context.clone(),
            config: Self::config(),
            provider: self.provider(),
            permission_mode: "bypass".into(),
            executable_override: Some(self.supervisor.clone()),
            gate,
        }
    }

    /// A fresh host process equivalent: a new service context over the same DB
    /// and data directory.
    fn reopen(&self) -> Launcher {
        let context = ServiceContext::open(
            RuntimePaths::new(self.data.clone()).unwrap(),
            Arc::new(EnvironmentCredentials),
        )
        .unwrap();
        Launcher {
            context,
            config: Self::config(),
            provider: self.provider(),
            permission_mode: "bypass".into(),
            executable_override: Some(self.supervisor.clone()),
            gate: None,
        }
    }

    /// Number of actual child invocations by the real supervisor. Authorized
    /// jobs are awaited to a terminal state first, so a failed recovery that
    /// starts a second supervisor is counted rather than raced with.
    fn child_runs(&self) -> usize {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            let jobs = background_sessions::list(&self.data, Some(&self.router)).unwrap();
            let pending = jobs.iter().any(|job| {
                job.launched && !matches!(job.status.as_str(), "completed" | "failed" | "cancelled")
            });
            if !pending {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "authorized jobs did not settle: {jobs:#?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        std::fs::read_to_string(&self.launch_log)
            .map(|text| text.lines().filter(|line| !line.trim().is_empty()).count())
            .unwrap_or(0)
    }

    fn jobs_for(&self, session: &str) -> Vec<background_sessions::Job> {
        background_sessions::list(&self.data, Some(&self.router))
            .unwrap()
            .into_iter()
            .filter(|job| job.session_id == session)
            .collect()
    }

    fn all_jobs(&self) -> Vec<background_sessions::Job> {
        background_sessions::list(&self.data, Some(&self.router)).unwrap()
    }

    fn child_sessions(&self) -> Vec<String> {
        self.context
            .db
            .with_conn(|conn| -> anyhow::Result<Vec<String>> {
                let mut statement =
                    conn.prepare("SELECT id FROM conversations WHERE id <> ?1 ORDER BY id")?;
                let rows = statement.query_map([&self.router], |row| row.get::<_, String>(0))?;
                Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
            })
            .unwrap()
            .unwrap()
    }

    fn operation(&self, member: MemberRole, kind: LaunchKind, purpose: &str) -> LaunchOperation {
        let launcher = self.launcher();
        let key = launcher.operation_key_for(&self.task(), member, kind, purpose);
        launcher
            .load_operation(&key)
            .unwrap()
            .expect("launch operation")
    }
}

struct CrashGate {
    at: &'static str,
    fired: AtomicBool,
}

impl CrashGate {
    fn check(&self, stage: &'static str) -> Result<()> {
        if stage == self.at && !self.fired.swap(true, Ordering::SeqCst) {
            anyhow::bail!("simulated host crash at {stage}");
        }
        Ok(())
    }
}

fn crash_gate(at: &'static str) -> LaunchGate {
    let gate = Arc::new(CrashGate {
        at,
        fired: AtomicBool::new(false),
    });
    Arc::new(move |stage| gate.check(stage))
}

fn assert_crash(error: anyhow::Error, stage: &str) {
    assert!(
        format!("{error:#}").contains(&format!("simulated host crash at {stage}")),
        "{error:#}"
    );
}

#[tokio::test]
async fn a_crash_before_the_reservation_leaves_no_side_effect() {
    let fixture = Fixture::new();
    let task = fixture.task();
    let error = fixture
        .launcher_with_gate(Some(crash_gate("before-reservation")))
        .launch_coder(&task)
        .await
        .unwrap_err();
    assert_crash(error, "before-reservation");

    assert!(fixture.task().coder.session_id.is_none());
    assert!(fixture.child_sessions().is_empty());
    assert_eq!(fixture.child_runs(), 0);
    let launcher = fixture.launcher();
    let key = launcher.operation_key(&task, MemberRole::Coder, LaunchKind::Initial);
    assert!(launcher.load_operation(&key).unwrap().is_none());

    let outcome = fixture.reopen().launch_coder(&task).await.unwrap();
    assert!(outcome.fresh && !outcome.in_progress);
    assert_eq!(fixture.child_sessions(), vec![outcome.session_id.clone()]);
    assert_eq!(fixture.child_runs(), 1);
    assert_eq!(fixture.jobs_for(&outcome.session_id).len(), 1);
    let operation = fixture.operation(MemberRole::Coder, LaunchKind::Initial, "");
    assert_eq!(operation.state, "completed");
    assert_eq!(operation.session_id, outcome.session_id);
    assert_eq!(operation.job_id, outcome.job_id);
}

#[tokio::test]
async fn a_crash_after_the_reservation_reuses_the_same_identity() {
    let fixture = Fixture::new();
    let task = fixture.task();
    let error = fixture
        .launcher_with_gate(Some(crash_gate("after-reservation")))
        .launch_coder(&task)
        .await
        .unwrap_err();
    assert_crash(error, "after-reservation");

    let reserved = fixture.operation(MemberRole::Coder, LaunchKind::Initial, "");
    assert_eq!(reserved.state, "reserved");
    assert!(reserved.handle.is_none());
    assert!(fixture.child_sessions().is_empty());
    assert!(!fixture
        .data
        .join("background-tasks")
        .join(&reserved.job_id)
        .exists());
    assert_eq!(fixture.child_runs(), 0);

    let outcome = fixture.reopen().launch_coder(&task).await.unwrap();
    assert_eq!(outcome.session_id, reserved.session_id);
    assert_eq!(outcome.job_id, reserved.job_id);
    assert!(outcome.fresh);
    assert_eq!(fixture.child_sessions(), vec![reserved.session_id.clone()]);
    assert_eq!(fixture.child_runs(), 1);
    let operation = fixture.operation(MemberRole::Coder, LaunchKind::Initial, "");
    assert_eq!(operation.state, "completed");
    assert_eq!(operation.session_id, reserved.session_id);
}

#[tokio::test]
async fn a_crash_after_session_allocation_continues_the_same_session() {
    let fixture = Fixture::new();
    let task = fixture.task();
    let error = fixture
        .launcher_with_gate(Some(crash_gate("after-session")))
        .launch_coder(&task)
        .await
        .unwrap_err();
    assert_crash(error, "after-session");

    let reserved = fixture.operation(MemberRole::Coder, LaunchKind::Initial, "");
    assert_eq!(reserved.state, "session");
    assert_eq!(
        fixture.child_sessions(),
        vec![reserved.session_id.clone()],
        "the reserved session row was published before the crash"
    );
    assert!(fixture.task().coder.session_id.is_none());
    assert!(!fixture
        .data
        .join("background-tasks")
        .join(&reserved.job_id)
        .exists());
    assert_eq!(fixture.child_runs(), 0);

    let outcome = fixture.reopen().launch_coder(&task).await.unwrap();
    assert_eq!(outcome.session_id, reserved.session_id);
    assert_eq!(outcome.job_id, reserved.job_id);
    assert_eq!(fixture.child_sessions().len(), 1);
    assert_eq!(fixture.child_runs(), 1);
    assert_eq!(
        fixture.task().coder.session_id.as_deref(),
        Some(outcome.session_id.as_str())
    );
}

#[tokio::test]
async fn a_crash_after_spawn_adopts_the_started_child_without_relaunching() {
    let fixture = Fixture::new();
    let task = fixture.task();
    let error = fixture
        .launcher_with_gate(Some(crash_gate("after-spawn")))
        .launch_coder(&task)
        .await
        .unwrap_err();
    assert_crash(error, "after-spawn");

    let operation = fixture.operation(MemberRole::Coder, LaunchKind::Initial, "");
    assert!(operation.handle.is_none());
    let job = background_sessions::get(&fixture.data, &operation.job_id).unwrap();
    assert!(job.launched, "the job must record the external side effect");
    assert_eq!(fixture.child_runs(), 1);
    assert_eq!(
        fixture.task().coder.session_id.as_deref(),
        Some(operation.session_id.as_str())
    );

    let outcome = fixture.reopen().launch_coder(&task).await.unwrap();
    assert!(outcome.fresh);
    assert_eq!(outcome.session_id, operation.session_id);
    assert_eq!(outcome.job_id, operation.job_id);
    assert_eq!(fixture.child_runs(), 1, "adoption must not spawn again");
    assert_eq!(fixture.jobs_for(&outcome.session_id).len(), 1);
    let operation = fixture.operation(MemberRole::Coder, LaunchKind::Initial, "");
    assert_eq!(operation.state, "completed");
    assert!(operation.handle.is_some());
    assert!(operation.consumed_at.is_some());
    assert_eq!(
        operation.last_job_status.as_deref(),
        Some(
            background_sessions::get(&fixture.data, &outcome.job_id)
                .unwrap()
                .status
                .as_str()
        ),
        "the durable job status is reconciled"
    );
}

#[tokio::test]
async fn a_crash_after_the_handle_record_completes_without_respawning() {
    let fixture = Fixture::new();
    let task = fixture.task();
    let error = fixture
        .launcher_with_gate(Some(crash_gate("after-record")))
        .launch_coder(&task)
        .await
        .unwrap_err();
    assert_crash(error, "after-record");

    let operation = fixture.operation(MemberRole::Coder, LaunchKind::Initial, "");
    assert_eq!(operation.state, "spawned");
    assert!(operation.handle.is_some());
    assert_eq!(fixture.child_runs(), 1);

    let outcome = fixture.reopen().launch_coder(&task).await.unwrap();
    assert!(!outcome.fresh, "a completed operation is cached");
    assert_eq!(outcome.session_id, operation.session_id);
    assert_eq!(fixture.child_runs(), 1);
    let operation = fixture.operation(MemberRole::Coder, LaunchKind::Initial, "");
    assert_eq!(operation.state, "completed");
}

#[tokio::test]
async fn a_resume_instruction_is_delivered_exactly_once_across_a_crash() {
    let fixture = Fixture::new();
    let task = fixture.task();
    let initial = fixture.launcher().launch_coder(&task).await.unwrap();
    assert_eq!(fixture.child_runs(), 1);
    let task = fixture.task();
    let instruction = "fix the failing test and commit";

    let error = fixture
        .launcher_with_gate(Some(crash_gate("after-spawn")))
        .resume_coder(&task, instruction, "implementation")
        .await
        .unwrap_err();
    assert_crash(error, "after-spawn");

    let reserved = fixture.operation(MemberRole::Coder, LaunchKind::Resume, "implementation");
    assert_eq!(reserved.kind, "resume");
    assert_eq!(
        reserved.resume_session.as_deref(),
        Some(initial.session_id.as_str())
    );
    assert!(reserved.handle.is_none());
    let job = background_sessions::get(&fixture.data, &reserved.job_id).unwrap();
    assert!(job.launched);
    let launch = background_sessions::launch_of(&fixture.data, &reserved.job_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        launch.arguments.last().map(String::as_str),
        Some(instruction)
    );
    assert_eq!(fixture.child_runs(), 2);
    assert_eq!(fixture.jobs_for(&initial.session_id).len(), 2);

    let resumed = fixture
        .reopen()
        .resume_coder(&task, instruction, "implementation")
        .await
        .unwrap();
    assert!(resumed.fresh);
    assert_eq!(resumed.session_id, initial.session_id);
    assert_eq!(resumed.job_id, reserved.job_id);
    assert_eq!(fixture.child_runs(), 2, "the instruction is not resent");
    assert_eq!(fixture.jobs_for(&initial.session_id).len(), 2);
    let operation = fixture.operation(MemberRole::Coder, LaunchKind::Resume, "implementation");
    assert_eq!(operation.state, "completed");
    assert!(operation.consumed_at.is_some());

    let again = fixture
        .reopen()
        .resume_coder(&task, instruction, "implementation")
        .await
        .unwrap();
    assert!(!again.fresh);
    assert_eq!(fixture.child_runs(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_duplicate_ticks_start_one_process() {
    let fixture = Fixture::new();
    let task = fixture.task();
    let first = fixture.launcher();
    let second = fixture.reopen();
    let first_task = task.clone();
    let second_task = task.clone();
    let (left, right) = tokio::join!(
        tokio::spawn(async move { first.launch_coder(&first_task).await }),
        tokio::spawn(async move { second.launch_coder(&second_task).await })
    );
    let left = left.unwrap().unwrap();
    let right = right.unwrap().unwrap();
    assert_eq!(left.session_id, right.session_id);
    assert_eq!(left.job_id, right.job_id);
    assert!(
        left.fresh || right.fresh,
        "at least one tick must launch the child"
    );
    assert_eq!(fixture.child_sessions(), vec![left.session_id.clone()]);
    assert_eq!(fixture.child_runs(), 1);
    assert_eq!(fixture.jobs_for(&left.session_id).len(), 1);
    assert_eq!(fixture.all_jobs().len(), 1);
    assert_eq!(
        fixture
            .operation(MemberRole::Coder, LaunchKind::Initial, "")
            .state,
        "completed"
    );
}

#[tokio::test]
async fn unreadable_reserved_job_state_blocks_and_is_preserved() {
    let fixture = Fixture::new();
    let task = fixture.task();
    fixture
        .launcher_with_gate(Some(crash_gate("after-reservation")))
        .launch_coder(&task)
        .await
        .unwrap_err();
    let reserved = fixture.operation(MemberRole::Coder, LaunchKind::Initial, "");
    let directory = background_sessions::directory(&fixture.data, &reserved.job_id).unwrap();
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("state.json"), b"{not-json").unwrap();

    let error = fixture.reopen().launch_coder(&task).await.unwrap_err();
    assert!(
        format!("{error:#}").contains("unreadable state"),
        "{error:#}"
    );
    assert_eq!(
        std::fs::read(directory.join("state.json")).unwrap(),
        b"{not-json"
    );
    assert_eq!(fixture.child_runs(), 0);
    assert!(fixture
        .operation(MemberRole::Coder, LaunchKind::Initial, "")
        .last_error
        .is_some());
}

#[tokio::test]
async fn a_reviewer_launch_uses_the_reviewer_member_not_the_coder_record() {
    let fixture = Fixture::new();
    let task = fixture.task();
    let coder = fixture.launcher().launch_coder(&task).await.unwrap();
    let head = git(&fixture.project, &["rev-parse", "HEAD"]);
    let mut repo = fixture.task().repositories[0].clone();
    repo.submitted_commit = Some(head);
    fixture.store.save_repo(&repo).unwrap();

    let task = fixture.task();
    let reviewer = fixture.launcher().launch_reviewer(&task).await.unwrap();
    assert_ne!(reviewer.session_id, coder.session_id);
    let task = fixture.task();
    assert_eq!(
        task.coder.session_id.as_deref(),
        Some(coder.session_id.as_str())
    );
    assert_eq!(
        task.reviewer.session_id.as_deref(),
        Some(reviewer.session_id.as_str())
    );
    assert_eq!(fixture.jobs_for(&coder.session_id).len(), 1);
    assert_eq!(fixture.jobs_for(&reviewer.session_id).len(), 1);
    assert_eq!(fixture.child_runs(), 2);
}

#[tokio::test]
async fn a_bound_member_session_is_never_replaced() {
    let fixture = Fixture::new();
    let task = fixture.task();
    let coder = fixture.launcher().launch_coder(&task).await.unwrap();
    fixture
        .store
        .replace_coder_session(&task.task_id, &uuid::Uuid::new_v4().to_string())
        .unwrap();
    let error = fixture
        .launcher()
        .launch_coder(&fixture.task())
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("already uses"), "{error:#}");
    assert_eq!(fixture.child_runs(), 1);
    assert_eq!(fixture.jobs_for(&coder.session_id).len(), 1);
}

#[tokio::test]
async fn the_next_review_round_resumes_the_same_reviewer_session() {
    let fixture = Fixture::new();
    let task = fixture.task();
    fixture.launcher().launch_coder(&task).await.unwrap();
    let head = git(&fixture.project, &["rev-parse", "HEAD"]);
    let mut repo = fixture.task().repositories[0].clone();
    repo.submitted_commit = Some(head);
    fixture.store.save_repo(&repo).unwrap();
    let task = fixture.task();
    let reviewer = fixture.launcher().launch_reviewer(&task).await.unwrap();
    assert_eq!(fixture.child_runs(), 2);

    // The host has advanced the review round; the same Reviewer session gets
    // exactly one new round operation.
    let mut next_round = fixture.task();
    next_round.review_round = 1;
    let resumed = fixture
        .launcher()
        .resume_reviewer(&next_round)
        .await
        .unwrap();
    assert!(resumed.fresh);
    assert!(resumed.resumed);
    assert_eq!(resumed.session_id, reviewer.session_id);
    assert_eq!(fixture.jobs_for(&reviewer.session_id).len(), 2);
    assert_eq!(fixture.child_runs(), 3);
    let again = fixture
        .launcher()
        .resume_reviewer(&next_round)
        .await
        .unwrap();
    assert!(!again.fresh);
    assert_eq!(fixture.child_runs(), 3);
}

#[tokio::test]
async fn a_failed_spawn_keeps_the_identity_and_is_retried() {
    let fixture = Fixture::new();
    let task = fixture.task();
    let missing = fixture._temp.path().join("no-such-supervisor");
    let launcher = Launcher {
        context: fixture.context.clone(),
        config: Fixture::config(),
        provider: fixture.provider(),
        permission_mode: "bypass".into(),
        executable_override: Some(missing),
        gate: None,
    };
    launcher.launch_coder(&task).await.unwrap_err();

    let operation = fixture.operation(MemberRole::Coder, LaunchKind::Initial, "");
    assert!(operation.handle.is_none());
    assert!(
        operation.last_error.is_some(),
        "the failed attempt retains its error without losing the identity"
    );
    assert_eq!(fixture.child_sessions(), vec![operation.session_id.clone()]);
    let job = background_sessions::get(&fixture.data, &operation.job_id).unwrap();
    assert!(!job.launched, "no process was recorded");
    assert_eq!(job.status, "failed");
    assert_eq!(fixture.child_runs(), 0);

    let outcome = fixture.reopen().launch_coder(&task).await.unwrap();
    assert!(outcome.fresh);
    assert_eq!(outcome.session_id, operation.session_id);
    assert_eq!(outcome.job_id, operation.job_id);
    assert_eq!(fixture.child_runs(), 1);
    assert!(
        background_sessions::get(&fixture.data, &operation.job_id)
            .unwrap()
            .launched
    );
}

/// A final-delivery review of the same spec version and review round must have
/// its own durable operation identity. Sharing the implementation review's key
/// would suppress the final launch or reuse its prompt.
#[test]
fn review_and_integration_operations_are_identified_by_evidence_stage() {
    let fixture = Fixture::new();
    let task = fixture.task();
    let launcher = fixture.launcher();

    let implementation = launcher.operation_key_for(
        &task,
        MemberRole::Reviewer,
        LaunchKind::Resume,
        "implementation",
    );
    let final_delivery = launcher.operation_key_for(
        &task,
        MemberRole::Reviewer,
        LaunchKind::Resume,
        "final-delivery",
    );
    assert_ne!(implementation, final_delivery);
    assert!(
        final_delivery.ends_with(".final-delivery"),
        "{final_delivery}"
    );
    assert!(
        implementation.ends_with(".implementation"),
        "{implementation}"
    );
    // The unqualified key stays the initial-launch identity.
    assert_eq!(
        launcher.operation_key(&task, MemberRole::Reviewer, LaunchKind::Resume),
        implementation.trim_end_matches(".implementation")
    );

    let coder_implementation = launcher.operation_key_for(
        &task,
        MemberRole::Coder,
        LaunchKind::Resume,
        "implementation",
    );
    let coder_changes =
        launcher.operation_key_for(&task, MemberRole::Coder, LaunchKind::Resume, "changes");
    let coder_integration =
        launcher.operation_key_for(&task, MemberRole::Coder, LaunchKind::Resume, "integration");
    assert_ne!(coder_implementation, coder_integration);
    assert_ne!(coder_implementation, coder_changes);
    assert_ne!(coder_changes, coder_integration);

    // A reservation recorded under one stage is never returned for another
    // stage's key, so a final review cannot adopt an implementation review.
    launcher
        .context
        .save_setting(
            &implementation,
            &json!({
                "operation_id": "task-001:reviewer:resume",
                "task_id": task.task_id,
                "role": "reviewer",
                "kind": "resume",
                "spec_version": task.spec_version,
                "review_round": task.review_round,
                "purpose": "implementation",
                "workspace": "/workspace",
                "session_id": "reviewer-session",
                "job_id": "implementation-job",
                "prompt": "implementation review",
                "model": "router-model",
                "state": "completed",
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-01T00:00:00Z"
            }),
        )
        .unwrap();
    assert!(launcher.load_operation(&implementation).unwrap().is_some());
    assert!(
        launcher.load_operation(&final_delivery).unwrap().is_none(),
        "the final review adopted the implementation review operation"
    );
}
