use super::*;
use crate::native_engine::provider_manager::ResolvedProvider;
use crate::runtime::{RuntimePaths, SessionStore};
use crate::super_agent::config::SuperAgentConfig;
use crate::super_agent::crp::{self, ToolRuntime};
use crate::super_agent::role::{self, Role};
use crate::super_agent::store::{ApprovalDraft, TaskStore};
use crate::super_agent::{EventType, NewEvent, TaskSpec};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

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

fn executable() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from("cmd.exe")
    } else {
        PathBuf::from("/bin/true")
    }
}

struct Fixture {
    _temp: tempfile::TempDir,
    data: PathBuf,
    repo: PathBuf,
    context: ServiceContext,
    router: String,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let repo = temp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "test@example.invalid"]);
        git(&repo, &["config", "user.name", "Test"]);
        std::fs::write(repo.join("file.txt"), "main\n").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-q", "-m", "initial"]);
        git(&repo, &["switch", "-q", "-c", "develop"]);
        std::fs::write(repo.join("file.txt"), "develop\n").unwrap();
        git(&repo, &["commit", "-q", "-am", "develop"]);
        git(&repo, &["switch", "-q", "main"]);
        let repo = repo.canonicalize().unwrap();
        let data = temp.path().join("data");
        let context = ServiceContext::open(
            RuntimePaths::new(data.clone()).unwrap(),
            Arc::new(crate::services::EnvironmentCredentials),
        )
        .unwrap();
        let router = SessionStore::new(context.db.clone())
            .create(temp.path(), "fixture", "fixture")
            .unwrap();
        role::set(&context.db, &router, Role::Router).unwrap();
        Self {
            _temp: temp,
            data,
            repo,
            context,
            router,
        }
    }

    fn store(&self) -> TaskStore {
        TaskStore::new(self.context.db.clone(), self.data.clone())
    }

    fn spec(&self) -> TaskSpec {
        TaskSpec {
            title: "Fixture task".into(),
            workspace: self._temp.path().canonicalize().unwrap(),
            targets: vec![self.repo.to_string_lossy().into_owned()],
            goal: "Complete the task".into(),
            requirements: vec!["Keep behavior".into()],
            acceptance_criteria: vec![crate::super_agent::AcceptanceCriterion {
                id: "AC-001".into(),
                description: "Tests pass".into(),
            }],
            constraints: vec![],
            out_of_scope: vec![],
            target_branch: Some("develop".into()),
            repository_target_branches: Default::default(),
        }
    }

    fn task(&self) -> TaskRecord {
        self.store()
            .create_task(&self.router, self.spec(), "create")
            .unwrap()
    }

    fn provider(&self) -> ResolvedProvider {
        ResolvedProvider {
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
        }
    }

    fn orchestrator(&self, config: Value) -> Orchestrator {
        let mut config = config;
        if config.get("agents").is_none() {
            config["agents"] = json!({});
        }
        let config: SuperAgentConfig = crate::super_agent::config::parse(&config).unwrap();
        let mut provider = self.provider();
        provider.provider.models = vec![provider.model.clone()];
        Orchestrator::new(
            self.context.clone(),
            config,
            provider,
            "bypass".into(),
            Some(executable()),
        )
    }

    fn runtime_for(&self, session: &str) -> ToolRuntime {
        ToolRuntime {
            db: self.context.db.clone(),
            data_dir: self.data.clone(),
            session_id: session.into(),
        }
    }

    fn coder_submits_plan(&self, task: &TaskRecord, session: &str) {
        let report = json!({
            "spec_version": task.spec_version,
            "round": task.review_round,
            "understanding": "Fixture",
            "scope": ["file.txt"],
            "steps": ["edit"],
            "verification": ["cargo test"],
            "risks": []
        });
        let value = crp::execute(
            &self.runtime_for(session),
            "CrpSubmit",
            json!({"kind":"plan","payload":report}),
            "plan-call",
        )
        .unwrap();
        assert_eq!(value["kind"], "PLAN");
    }

    fn launch_jobs(&self, task: &TaskRecord) -> usize {
        crate::services::background_sessions::list(&self.data, Some(&self.router))
            .unwrap()
            .into_iter()
            .filter(|job| job.session_id == task.coder.session_id.clone().unwrap_or_default())
            .count()
    }
}

#[tokio::test]
async fn manual_plan_approval_launches_then_resumes_the_same_coder_once() {
    let fixture = Fixture::new();
    let task = fixture.task();
    let orchestrator = fixture.orchestrator(json!({"approvals":{"plan":"manual"}}));

    let first = orchestrator.tick(&fixture.router).await.unwrap();
    assert_eq!(first.blocked, Vec::<String>::new(), "{first:?}");
    assert!(first.launched.iter().any(|line| line.contains("coder")));
    let task = fixture.store().get_task(&task.task_id).unwrap().unwrap();
    assert_eq!(task.phase, Phase::Planning);
    assert_eq!(task.status, Status::Running);
    assert_eq!(task.repositories.len(), 1);
    assert_eq!(task.repositories[0].target_branch, "develop");
    let coder = task.coder.session_id.clone().unwrap();
    assert_eq!(
        role::membership(&fixture.context.db, &coder).unwrap(),
        Some((task.task_id.clone(), crate::super_agent::MemberRole::Coder))
    );
    let conversation = fixture
        .context
        .db
        .with_conn(|conn| crate::db::conversation_repo::get_conversation(conn, &coder))
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        conversation.workspace_path.as_deref(),
        task.repositories[0]
            .working_path
            .to_str()
            .map(str::to_owned)
            .as_deref()
    );
    assert_eq!(fixture.launch_jobs(&task), 1);

    // A second tick before the plan exists changes nothing.
    let idle = orchestrator.tick(&fixture.router).await.unwrap();
    assert!(idle.launched.is_empty(), "{idle:?}");
    assert!(idle.approvals.is_empty(), "{idle:?}");
    assert_eq!(fixture.launch_jobs(&task), 1);

    fixture.coder_submits_plan(&task, &coder);
    let second = orchestrator.tick(&fixture.router).await.unwrap();
    assert_eq!(second.approvals.len(), 1, "{second:?}");
    let task = fixture.store().get_task(&task.task_id).unwrap().unwrap();
    assert_eq!(task.status, Status::WaitingUser);
    assert_eq!(task.phase, Phase::Planning);
    assert_eq!(
        fixture.launch_jobs(&task),
        1,
        "manual approval must not resume early"
    );

    // Repeating ticks must not create a second approval or job.
    let repeated = orchestrator.tick(&fixture.router).await.unwrap();
    assert!(repeated.approvals.is_empty(), "{repeated:?}");
    assert!(repeated.launched.is_empty(), "{repeated:?}");
    assert_eq!(fixture.store().approvals(&task.task_id).unwrap().len(), 1);

    let approval = fixture.store().approvals(&task.task_id).unwrap().remove(0);
    let answered = crp::execute(
        &fixture.runtime_for(&fixture.router),
        "CrpAnswer",
        json!({"approval_id": approval.approval_id, "decision":"approved"}),
        "answer",
    )
    .unwrap();
    assert_eq!(answered["status"], "APPROVED");

    let fourth = orchestrator.tick(&fixture.router).await.unwrap();
    assert!(
        fourth
            .launched
            .iter()
            .any(|line| line.contains("(resumed)")),
        "{fourth:?}"
    );
    let task = fixture.store().get_task(&task.task_id).unwrap().unwrap();
    assert_eq!(task.phase, Phase::Implementing);
    assert_eq!(task.status, Status::Running);
    assert_eq!(task.coder.session_id.as_deref(), Some(coder.as_str()));
    assert_eq!(fixture.launch_jobs(&task), 2);

    // The resume is marker-keyed: another tick does not resume again.
    let fifth = orchestrator.tick(&fixture.router).await.unwrap();
    assert!(
        !fifth.launched.iter().any(|line| line.contains("(resumed)")),
        "{fifth:?}"
    );
    assert_eq!(fixture.launch_jobs(&task), 2);
}

#[tokio::test]
async fn auto_policy_approves_and_resumes_within_one_tick() {
    let fixture = Fixture::new();
    let task = fixture.task();
    let orchestrator = fixture.orchestrator(json!({"approvals":{"plan":"auto"}}));
    orchestrator.tick(&fixture.router).await.unwrap();
    let task = fixture.store().get_task(&task.task_id).unwrap().unwrap();
    let coder = task.coder.session_id.clone().unwrap();
    fixture.coder_submits_plan(&task, &coder);

    let report = orchestrator.tick(&fixture.router).await.unwrap();
    assert_eq!(report.warnings, Vec::<String>::new());
    let task = fixture.store().get_task(&task.task_id).unwrap().unwrap();
    assert_eq!(task.phase, Phase::Implementing);
    assert_eq!(task.coder.session_id.as_deref(), Some(coder.as_str()));
    let approvals = fixture.store().approvals(&task.task_id).unwrap();
    assert_eq!(approvals.len(), 1);
    assert_eq!(
        approvals[0].status,
        crate::super_agent::ApprovalStatus::Approved
    );
    assert_eq!(fixture.launch_jobs(&task), 2);
}

#[tokio::test]
async fn a_missing_configured_model_falls_back_and_reports_a_warning() {
    let fixture = Fixture::new();
    let task = fixture.task();
    let orchestrator = fixture.orchestrator(json!({"agents":{"coder":{"model":"missing-model"}}}));
    let report = orchestrator.tick(&fixture.router).await.unwrap();
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("missing-model")),
        "{report:?}"
    );
    let task = fixture.store().get_task(&task.task_id).unwrap().unwrap();
    let record: crate::super_agent::launcher::LaunchRecord = fixture
        .context
        .setting(&format!("super-agent.launch.{}.coder", task.task_id))
        .unwrap()
        .unwrap();
    assert_eq!(record.model, "router-model");
    assert!(record.warning.unwrap().contains("missing-model"));
}

#[tokio::test]
async fn the_reviewer_gets_a_pinned_checkout_separate_from_the_coder() {
    let fixture = Fixture::new();
    let task = fixture.task();
    let orchestrator = fixture.orchestrator(json!({"approvals":{"plan":"auto"}}));
    orchestrator.tick(&fixture.router).await.unwrap();
    let mut task = fixture.store().get_task(&task.task_id).unwrap().unwrap();
    let coder = task.coder.session_id.clone().unwrap();
    fixture.coder_submits_plan(&task, &coder);
    orchestrator.tick(&fixture.router).await.unwrap();
    task = fixture.store().get_task(&task.task_id).unwrap().unwrap();
    assert_eq!(task.phase, Phase::Implementing);

    // The Coder commits its implementation in the task workspace.
    let workspace = task.repositories[0].working_path.clone();
    std::fs::write(workspace.join("file.txt"), "implemented\n").unwrap();
    git(&workspace, &["commit", "-q", "-am", "implement"]);
    let commit = git(&workspace, &["rev-parse", "HEAD"]);
    let mut repo = task.repositories[0].clone();
    repo.submitted_commit = Some(commit.clone());
    fixture.store().save_repo(&repo).unwrap();
    fixture
        .store()
        .apply_event(
            &task.task_id,
            NewEvent {
                session_id: Some(coder.clone()),
                run_id: None,
                event_type: EventType::ImplementationSubmitted,
                causation_id: None,
                operation_id: "implementation".into(),
                payload: json!({}),
            },
            &crate::super_agent::StatePolicy::default(),
        )
        .unwrap();

    let report = orchestrator.tick(&fixture.router).await.unwrap();
    assert!(
        report.launched.iter().any(|line| line.contains("reviewer")),
        "{report:?}"
    );
    let task = fixture.store().get_task(&task.task_id).unwrap().unwrap();
    let reviewer = task.reviewer.session_id.clone().unwrap();
    assert_eq!(
        role::membership(&fixture.context.db, &reviewer).unwrap(),
        Some((
            task.task_id.clone(),
            crate::super_agent::MemberRole::Reviewer
        ))
    );
    let conversation = fixture
        .context
        .db
        .with_conn(|conn| crate::db::conversation_repo::get_conversation(conn, &reviewer))
        .unwrap()
        .unwrap()
        .unwrap();
    let review_workspace = PathBuf::from(conversation.workspace_path.unwrap());
    assert_ne!(review_workspace, task.repositories[0].working_path);
    assert_eq!(git(&review_workspace, &["rev-parse", "HEAD"]), commit);
    assert_eq!(
        std::fs::read_to_string(review_workspace.join("file.txt")).unwrap(),
        "implemented\n"
    );
    // The Reviewer cannot write into the Coder's checkout: they are separate.
    assert_ne!(
        review_workspace.canonicalize().unwrap(),
        workspace.canonicalize().unwrap()
    );
}

#[tokio::test]
async fn an_unusable_target_blocks_the_task_without_creating_a_coder() {
    let fixture = Fixture::new();
    let mut spec = fixture.spec();
    spec.targets = vec!["/definitely/outside/the/workspace".into()];
    let task = fixture
        .store()
        .create_task(&fixture.router, spec, "create")
        .unwrap();
    let orchestrator = fixture.orchestrator(json!({}));
    let report = orchestrator.tick(&fixture.router).await.unwrap();
    assert!(!report.blocked.is_empty(), "{report:?}");
    let task = fixture.store().get_task(&task.task_id).unwrap().unwrap();
    assert_eq!(task.status, Status::Blocked);
    assert!(task.coder.session_id.is_none());
    assert!(task.repositories.is_empty());
    assert!(
        crate::services::background_sessions::list(&fixture.data, Some(&fixture.router))
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn events_from_another_router_are_not_processed_by_this_tick() {
    let fixture = Fixture::new();
    let other_context = ServiceContext::open(
        RuntimePaths::new(fixture.data.clone()).unwrap(),
        Arc::new(crate::services::EnvironmentCredentials),
    )
    .unwrap();
    let other = SessionStore::new(other_context.db.clone())
        .create(fixture._temp.path(), "fixture", "fixture")
        .unwrap();
    let task = fixture
        .store()
        .create_task(&other, fixture.spec(), "create")
        .unwrap();
    let orchestrator = fixture.orchestrator(json!({}));
    let report = orchestrator.tick(&fixture.router).await.unwrap();
    assert!(report.launched.is_empty(), "{report:?}");
    let task = fixture.store().get_task(&task.task_id).unwrap().unwrap();
    assert_eq!(task.phase, Phase::Discovering);
    assert!(task.coder.session_id.is_none());
}

#[test]
fn approval_draft_requires_the_current_contract() {
    // The orchestrator always sends the current versions; this guards against a
    // future caller passing stale values by mistake.
    let fixture = Fixture::new();
    let task = fixture.task();
    let draft = ApprovalDraft {
        source_session_id: "coder".into(),
        parent_session_id: fixture.router.clone(),
        request_type: crate::super_agent::ApprovalType::Plan,
        request: json!({}),
        task_spec_version: task.spec_version + 1,
        permission_policy_version: task.permission_policy_version,
    };
    let approval = fixture
        .store()
        .request_approval("stale", &task.task_id, draft)
        .unwrap();
    assert_eq!(approval.task_spec_version, task.spec_version + 1);
}

fn external_repository(root: &Path) -> PathBuf {
    std::fs::create_dir_all(root).unwrap();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.email", "test@example.invalid"]);
    git(root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("file.txt"), "outside\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-q", "-m", "initial"]);
    git(root, &["switch", "-q", "-c", "develop"]);
    root.canonicalize().unwrap()
}

#[tokio::test]
async fn a_self_declared_external_workspace_is_rejected_before_any_mutation() {
    let fixture = Fixture::new();
    let outside = tempfile::tempdir().unwrap();
    let repo = external_repository(&outside.path().join("repo"));
    let mut spec = fixture.spec();
    spec.workspace = outside.path().to_path_buf();
    spec.targets = vec![repo.to_string_lossy().into_owned()];
    let task = fixture
        .store()
        .create_task(&fixture.router, spec, "create-external")
        .unwrap();
    let orchestrator = fixture.orchestrator(json!({}));
    let report = orchestrator.tick(&fixture.router).await.unwrap();
    assert!(!report.blocked.is_empty(), "{report:?}");
    let task = fixture.store().get_task(&task.task_id).unwrap().unwrap();
    assert!(task.repositories.is_empty());
    assert!(task.non_git_targets.is_empty());
    assert!(task.coder.session_id.is_none());
    assert!(!fixture.data.join("super-agent").join(&task.task_id).exists());
    assert_eq!(
        std::fs::read_to_string(repo.join("file.txt")).unwrap(),
        "outside\n"
    );
}

#[tokio::test]
async fn an_explicitly_granted_additional_directory_is_accepted() {
    let fixture = Fixture::new();
    let outside = tempfile::tempdir().unwrap();
    let repo = external_repository(&outside.path().join("repo"));
    std::fs::write(
        fixture.data.join("settings.json"),
        serde_json::to_vec(&json!({
            "permissions": {"additionalDirectories": [outside.path().to_string_lossy()]}
        }))
        .unwrap(),
    )
    .unwrap();
    let mut spec = fixture.spec();
    spec.workspace = outside.path().to_path_buf();
    spec.targets = vec![repo.to_string_lossy().into_owned()];
    let task = fixture
        .store()
        .create_task(&fixture.router, spec, "create-granted")
        .unwrap();
    let orchestrator = fixture.orchestrator(json!({}));
    let report = orchestrator.tick(&fixture.router).await.unwrap();
    assert_eq!(report.blocked, Vec::<String>::new(), "{report:?}");
    let task = fixture.store().get_task(&task.task_id).unwrap().unwrap();
    assert_eq!(task.repositories.len(), 1);
    assert!(task.coder.session_id.is_some(), "{task:?}");
}

async fn reviewing_task(fixture: &Fixture) -> (TaskRecord, String) {
    let task = fixture.task();
    let orchestrator = fixture.orchestrator(json!({}));
    let report = orchestrator.tick(&fixture.router).await.unwrap();
    assert_eq!(report.blocked, Vec::<String>::new(), "{report:?}");
    let store = fixture.store();
    let policy = StatePolicy::default();
    for (event_type, payload) in [
        (
            EventType::ApprovalRequested,
            json!({"request_type":"PLAN"}),
        ),
        (
            EventType::ApprovalResolved,
            json!({"request_type":"PLAN","decision":"APPROVED"}),
        ),
        (EventType::ImplementationSubmitted, json!({})),
    ] {
        store
            .apply_event(
                &task.task_id,
                NewEvent {
                    session_id: None,
                    run_id: None,
                    event_type,
                    causation_id: None,
                    operation_id: format!("fixture-{}", event_type.as_str()),
                    payload,
                },
                &policy,
            )
            .unwrap();
    }
    let task = store.get_task(&task.task_id).unwrap().unwrap();
    assert_eq!(task.phase, Phase::Reviewing);
    let report = orchestrator.tick(&fixture.router).await.unwrap();
    assert_eq!(report.blocked, Vec::<String>::new(), "{report:?}");
    let task = store.get_task(&task.task_id).unwrap().unwrap();
    let reviewer = task.reviewer.session_id.clone().unwrap();
    assert!(report.launched.iter().any(|line| line.contains("reviewer")), "{report:?}");
    (task, reviewer)
}

fn submit_review(fixture: &Fixture, reviewer: &str, task: &TaskRecord, verdict: &str) {
    let value = crp::execute(
        &fixture.runtime_for(reviewer),
        "CrpSubmit",
        json!({"kind":"review","payload":{
            "verdict": verdict,
            "spec_version": task.spec_version,
            "round": task.review_round,
            "stage": "IMPLEMENTATION",
            "reviewed_versions": [],
            "acceptance_results": [],
            "issues": [{"severity":"LOW","location":"file.txt","description":"missing test","expected":"add a test"}],
            "verification": [],
            "limitations": []
        }}),
        &format!("review-{verdict}-{}", task.review_round),
    )
    .unwrap();
    assert_eq!(value["kind"], "REVIEW");
}

fn submit_harness(fixture: &Fixture, reviewer: &str, task: &TaskRecord, findings: Value) {
    crp::execute(
        &fixture.runtime_for(reviewer),
        "CrpSubmit",
        json!({"kind":"harness","payload":{
            "spec_version": task.spec_version,
            "round": task.review_round,
            "findings": findings,
            "notes": []
        }}),
        &format!("harness-{}", task.review_round),
    )
    .unwrap();
}

#[tokio::test]
async fn a_review_waits_for_the_harness_report_in_either_arrival_order() {
    for review_first in [true, false] {
        let fixture = Fixture::new();
        let (task, reviewer) = reviewing_task(&fixture).await;
        let orchestrator = fixture.orchestrator(json!({}));
        if review_first {
            submit_review(&fixture, &reviewer, &task, "APPROVED");
            let report = orchestrator.tick(&fixture.router).await.unwrap();
            assert_eq!(report.blocked, Vec::<String>::new(), "{report:?}");
            let current = fixture.store().get_task(&task.task_id).unwrap().unwrap();
            assert_eq!(current.phase, Phase::Reviewing);
            assert!(!fixture.store().events(&task.task_id).unwrap().iter().any(
                |event| event.event_type == EventType::ReviewCompleted
            ));
            submit_harness(&fixture, &reviewer, &task, json!([]));
        } else {
            submit_harness(&fixture, &reviewer, &task, json!([]));
            let report = orchestrator.tick(&fixture.router).await.unwrap();
            assert_eq!(report.blocked, Vec::<String>::new(), "{report:?}");
            let current = fixture.store().get_task(&task.task_id).unwrap().unwrap();
            assert_eq!(current.phase, Phase::Reviewing);
            submit_review(&fixture, &reviewer, &task, "APPROVED");
        }
        let report = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(report.blocked, Vec::<String>::new(), "{report:?}");
        let current = fixture.store().get_task(&task.task_id).unwrap().unwrap();
        assert_eq!(current.phase, Phase::Integrating);
        assert!(fixture.store().events(&task.task_id).unwrap().iter().any(
            |event| event.event_type == EventType::ReviewCompleted
                && event.payload["verdict"] == "APPROVED"
        ));
    }
}

#[tokio::test]
async fn changes_requested_resumes_the_same_coder_with_round_and_issues() {
    let fixture = Fixture::new();
    let (task, reviewer) = reviewing_task(&fixture).await;
    let original_coder = task.coder.session_id.clone().unwrap();
    submit_review(&fixture, &reviewer, &task, "CHANGES_REQUESTED");
    submit_harness(&fixture, &reviewer, &task, json!([]));
    let orchestrator = fixture.orchestrator(json!({}));
    let report = orchestrator.tick(&fixture.router).await.unwrap();
    assert_eq!(report.blocked, Vec::<String>::new(), "{report:?}");
    let current = fixture.store().get_task(&task.task_id).unwrap().unwrap();
    assert_eq!(current.phase, Phase::Implementing);
    assert_eq!(current.review_round, task.review_round + 1);
    assert_eq!(current.coder.session_id.as_deref(), Some(original_coder.as_str()));
    assert!(report
        .launched
        .iter()
        .any(|line| line.contains("(changes)")), "{report:?}");
    let marker = format!(
        "super-agent.resume-changes.{}.{}.{}",
        task.task_id, task.spec_version, current.review_round
    );
    assert!(fixture.context.setting::<Value>(&marker).unwrap().is_some());

    // The corrected implementation is pinned again and the same Reviewer is
    // resumed for the new round.
    let worktree = current.repositories[0].working_path.clone();
    std::fs::write(worktree.join("file.txt"), "fixed\n").unwrap();
    git(&worktree, &["add", "."]);
    git(&worktree, &["commit", "-qm", "fix review issues"]);
    crp::execute(
        &fixture.runtime_for(&original_coder),
        "CrpSubmit",
        json!({"kind":"implementation","payload":{
            "spec_version": current.spec_version,
            "round": current.review_round,
            "summary": "fixed"
        }}),
        "implementation-round-1",
    )
    .unwrap();
    let report = orchestrator.tick(&fixture.router).await.unwrap();
    assert_eq!(report.blocked, Vec::<String>::new(), "{report:?}");
    let reviewing = fixture.store().get_task(&task.task_id).unwrap().unwrap();
    assert_eq!(reviewing.phase, Phase::Reviewing);
    assert_eq!(reviewing.reviewer.session_id, current.reviewer.session_id);
    assert!(report
        .launched
        .iter()
        .any(|line| line.contains("reviewer") && line.contains("round 1")), "{report:?}");
    let reviewer_marker = format!(
        "super-agent.reviewer-resume.{}.{}.{}",
        task.task_id, reviewing.spec_version, reviewing.review_round
    );
    assert!(fixture
        .context
        .setting::<Value>(&reviewer_marker)
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn a_blocking_harness_finding_creates_a_delivery_blocker_approval() {
    let fixture = Fixture::new();
    let (task, reviewer) = reviewing_task(&fixture).await;
    submit_review(&fixture, &reviewer, &task, "APPROVED");
    submit_harness(
        &fixture,
        &reviewer,
        &task,
        json!([{
            "category":"HARNESS",
            "severity":"CRITICAL",
            "phenomenon":"The harness lost tool output",
            "create_task": true
        }]),
    );
    let orchestrator = fixture.orchestrator(json!({}));
    let report = orchestrator.tick(&fixture.router).await.unwrap();
    assert_eq!(report.blocked, Vec::<String>::new(), "{report:?}");
    let current = fixture.store().get_task(&task.task_id).unwrap().unwrap();
    assert_eq!(current.status, Status::Blocked);
    let approvals = fixture.store().approvals(&task.task_id).unwrap();
    assert!(approvals.iter().any(|approval| {
        approval.request_type == crate::super_agent::ApprovalType::DeliveryBlocker
            && approval.status == crate::super_agent::ApprovalStatus::Pending
    }));
    assert!(fixture.store().events(&task.task_id).unwrap().iter().any(
        |event| event.event_type == EventType::NewTaskProposed
    ));
}

#[tokio::test]
async fn efficiency_findings_do_not_block_an_approved_review() {
    let fixture = Fixture::new();
    let (task, reviewer) = reviewing_task(&fixture).await;
    submit_review(&fixture, &reviewer, &task, "APPROVED");
    submit_harness(
        &fixture,
        &reviewer,
        &task,
        json!([{
            "category":"MODEL",
            "severity":"CRITICAL",
            "phenomenon":"The model searched twice"
        }]),
    );
    let orchestrator = fixture.orchestrator(json!({}));
    let report = orchestrator.tick(&fixture.router).await.unwrap();
    assert_eq!(report.blocked, Vec::<String>::new(), "{report:?}");
    let current = fixture.store().get_task(&task.task_id).unwrap().unwrap();
    assert_eq!(current.phase, Phase::Integrating);
}

fn transition_to_reviewing(fixture: &Fixture, task_id: &str) {
    let store = fixture.store();
    let policy = StatePolicy::default();
    for (event_type, payload) in [
        (
            EventType::ApprovalRequested,
            json!({"request_type":"PLAN"}),
        ),
        (
            EventType::ApprovalResolved,
            json!({"request_type":"PLAN","decision":"APPROVED"}),
        ),
        (EventType::ImplementationSubmitted, json!({})),
    ] {
        store
            .apply_event(
                task_id,
                NewEvent {
                    session_id: None,
                    run_id: None,
                    event_type,
                    causation_id: None,
                    operation_id: format!("transition-{}", event_type.as_str()),
                    payload,
                },
                &policy,
            )
            .unwrap();
    }
}

#[tokio::test]
async fn a_dirty_submission_is_refused_before_the_commit_is_pinned() {
    let fixture = Fixture::new();
    let task = fixture.task();
    let orchestrator = fixture.orchestrator(json!({}));
    let report = orchestrator.tick(&fixture.router).await.unwrap();
    assert_eq!(report.blocked, Vec::<String>::new(), "{report:?}");
    let task = fixture.store().get_task(&task.task_id).unwrap().unwrap();
    let worktree = task.repositories[0].working_path.clone();
    std::fs::write(worktree.join("file.txt"), "uncommitted\n").unwrap();
    transition_to_reviewing(&fixture, &task.task_id);
    let report = orchestrator.tick(&fixture.router).await.unwrap();
    assert!(
        report
            .blocked
            .iter()
            .any(|line| line.contains("uncommitted changes")),
        "{report:?}"
    );
    let task = fixture.store().get_task(&task.task_id).unwrap().unwrap();
    assert_eq!(task.status, Status::Blocked);
    assert!(task.repositories[0].submitted_commit.is_none());
    assert!(task.reviewer.session_id.is_none());
}

#[tokio::test]
async fn a_pending_launch_intent_blocks_a_duplicate_resume() {
    let fixture = Fixture::new();
    let task = fixture.task();
    let orchestrator = fixture.orchestrator(json!({}));
    let report = orchestrator.tick(&fixture.router).await.unwrap();
    assert_eq!(report.blocked, Vec::<String>::new(), "{report:?}");
    let task = fixture.store().get_task(&task.task_id).unwrap().unwrap();
    let jobs_before = fixture.launch_jobs(&task);
    let policy = StatePolicy::default();
    let approval = fixture
        .store()
        .request_approval(
            "fixture-plan",
            &task.task_id,
            ApprovalDraft {
                source_session_id: task.coder.session_id.clone().unwrap(),
                parent_session_id: fixture.router.clone(),
                request_type: crate::super_agent::ApprovalType::Plan,
                request: json!({"round": task.review_round}),
                task_spec_version: task.spec_version,
                permission_policy_version: task.permission_policy_version,
            },
        )
        .unwrap();
    fixture
        .store()
        .resolve_approval(
            "fixture-plan-answer",
            &approval.approval_id,
            crate::super_agent::ApprovalStatus::Approved,
            None,
        )
        .unwrap();
    for (event_type, payload) in [
        (
            EventType::ApprovalRequested,
            json!({"request_type":"PLAN"}),
        ),
        (
            EventType::ApprovalResolved,
            json!({"request_type":"PLAN","decision":"APPROVED"}),
        ),
    ] {
        fixture
            .store()
            .apply_event(
                &task.task_id,
                NewEvent {
                    session_id: None,
                    run_id: None,
                    event_type,
                    causation_id: None,
                    operation_id: format!("approval-{}", event_type.as_str()),
                    payload,
                },
                &policy,
            )
            .unwrap();
    }
    let key = format!(
        "super-agent.launch-intent.{}.coder.resume.{}.{}",
        task.task_id, task.spec_version, task.review_round
    );
    fixture
        .context
        .save_setting(
            &key,
            &json!({"role":"coder","workspace":task.repositories[0].working_path,"state":"pending","at":"now"}),
        )
        .unwrap();
    let report = orchestrator.tick(&fixture.router).await.unwrap();
    assert!(
        report
            .blocked
            .iter()
            .any(|line| line.contains("launch")),
        "{report:?}"
    );
    let task = fixture.store().get_task(&task.task_id).unwrap().unwrap();
    assert_eq!(fixture.launch_jobs(&task), jobs_before);
}
