use super::*;
use crate::native_engine::provider_manager::ResolvedProvider;
use crate::runtime::{RuntimePaths, SessionStore};
use crate::super_agent::config::SuperAgentConfig;
use crate::super_agent::crp::{self, ToolRuntime};
use crate::super_agent::role::{self, Role};
use crate::super_agent::store::{ApprovalDraft, TaskStore};
use crate::super_agent::delivery::{
    DeliveryDestination, DeliveryDispatch, DeliveryOperationIdentity, DeliveryPlan,
    DeliveryTargetPlan,
};
use crate::super_agent::launcher::DeliveryJobBinding;
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

    fn launcher(&self) -> crate::super_agent::launcher::Launcher {
        let config: SuperAgentConfig =
            crate::super_agent::config::parse(&json!({"agents":{}})).unwrap();
        let mut provider = self.provider();
        provider.provider.models = vec![provider.model.clone()];
        crate::super_agent::launcher::Launcher {
            context: self.context.clone(),
            config,
            provider,
            permission_mode: "bypass".into(),
            executable_override: Some(executable()),
            gate: None,
        }
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
    assert!(!fixture
        .data
        .join("super-agent")
        .join(&task.task_id)
        .exists());
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
        (EventType::ApprovalRequested, json!({"request_type":"PLAN"})),
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
    assert!(
        report.launched.iter().any(|line| line.contains("reviewer")),
        "{report:?}"
    );
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
            assert!(!fixture
                .store()
                .events(&task.task_id)
                .unwrap()
                .iter()
                .any(|event| event.event_type == EventType::ReviewCompleted));
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
        assert!(fixture
            .store()
            .events(&task.task_id)
            .unwrap()
            .iter()
            .any(|event| event.event_type == EventType::ReviewCompleted
                && event.payload["verdict"] == "APPROVED"));
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
    assert_eq!(
        current.coder.session_id.as_deref(),
        Some(original_coder.as_str())
    );
    assert!(
        report
            .launched
            .iter()
            .any(|line| line.contains("(changes)")),
        "{report:?}"
    );
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
    assert!(
        report
            .launched
            .iter()
            .any(|line| line.contains("reviewer") && line.contains("round 1")),
        "{report:?}"
    );
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
    assert!(fixture
        .store()
        .events(&task.task_id)
        .unwrap()
        .iter()
        .any(|event| event.event_type == EventType::NewTaskProposed));
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
        (EventType::ApprovalRequested, json!({"request_type":"PLAN"})),
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
async fn an_interrupted_launch_is_adopted_on_the_next_tick() {
    let fixture = Fixture::new();
    let task = fixture.task();
    let orchestrator = fixture.orchestrator(json!({}));
    orchestrator.tick(&fixture.router).await.unwrap();
    let task = fixture.store().get_task(&task.task_id).unwrap().unwrap();
    let coder = task.coder.session_id.clone().unwrap();
    let jobs = fixture.launch_jobs(&task);
    assert_eq!(jobs, 1);

    // Simulate a host crash after the child was spawned but before the launch
    // handle was recorded. The task is already bound to the Coder session, so
    // the old `session_id.is_none()` gate would never retry it.
    let mut provider = fixture.provider();
    provider.provider.models = vec![provider.model.clone()];
    let launcher = crate::super_agent::launcher::Launcher {
        context: fixture.context.clone(),
        config: crate::super_agent::config::parse(&json!({"agents":{}})).unwrap(),
        provider,
        permission_mode: "bypass".into(),
        executable_override: Some(executable()),
        gate: None,
    };
    let key = launcher.operation_key(
        &task,
        crate::super_agent::MemberRole::Coder,
        crate::super_agent::launcher::LaunchKind::Initial,
    );
    let mut operation = launcher.load_operation(&key).unwrap().unwrap();
    operation.handle = None;
    operation.state = "session".into();
    fixture.context.save_setting(&key, &operation).unwrap();

    let report = orchestrator.tick(&fixture.router).await.unwrap();
    assert_eq!(report.blocked, Vec::<String>::new(), "{report:?}");
    assert!(
        report.launched.iter().any(|line| line.contains("coder")),
        "the interrupted launch must be adopted: {report:?}"
    );
    let task = fixture.store().get_task(&task.task_id).unwrap().unwrap();
    assert_eq!(task.coder.session_id.as_deref(), Some(coder.as_str()));
    assert_eq!(fixture.launch_jobs(&task), jobs, "no duplicate job");
    let operation = launcher.load_operation(&key).unwrap().unwrap();
    assert!(operation.handle.is_some());
    assert_eq!(operation.state, "completed");
}

/// The final delivery review must be decided only by FINAL_DELIVERY evidence.
/// An earlier implementation review and harness of the same spec/round stay in
/// history but cannot approve the integrated content.
#[test]
fn final_review_ignores_implementation_stage_evidence() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let task = fixture.task();
    let repo = crate::super_agent::RepoTarget {
        task_id: task.task_id.clone(),
        repository_id: "repo-code".into(),
        repo_root: fixture.repo.clone(),
        git_common_dir: fixture.repo.join(".git"),
        target_path: fixture.repo.clone(),
        target_branch: "develop".into(),
        task_branch: "agent/task".into(),
        worktree_path: fixture.repo.join("wt"),
        working_path: fixture.repo.join("wt"),
        base_commit: "base".into(),
        submitted_commit: Some("submitted".into()),
        reviewed_commit: Some("integrated".into()),
        integrated_commit: Some("integrated".into()),
        integration_status: crate::super_agent::IntegrationStatus::Prepared,
        verification_status: crate::super_agent::VerificationStatus::Passed,
        source_state: vec![],
    };
    store.save_repo(&repo).unwrap();
    let orchestrator = fixture.orchestrator(json!({}));
    let policy = orchestrator.policy();
    let apply = |event_type, operation: &str, payload: Value| {
        store
            .apply_event(
                &task.task_id,
                NewEvent {
                    session_id: None,
                    run_id: None,
                    event_type,
                    causation_id: None,
                    operation_id: operation.into(),
                    payload,
                },
                &policy,
            )
            .unwrap();
    };
    apply(EventType::DiscoveryCompleted, "discovery", json!({}));
    apply(EventType::PlanSubmitted, "plan", json!({}));
    apply(
        EventType::ApprovalRequested,
        "approval-request",
        json!({"request_type":"PLAN"}),
    );
    apply(
        EventType::ApprovalResolved,
        "approval-resolve",
        json!({"request_type":"PLAN","decision":"APPROVED"}),
    );
    apply(
        EventType::ImplementationSubmitted,
        "implementation",
        json!({}),
    );

    let implementation_review = json!({
        "verdict":"APPROVED","spec_version":1,"round":0,"stage":"IMPLEMENTATION",
        "reviewed_versions":[{"repository_id":"repo-code","commit":"submitted"}],
        "acceptance_results":[{"criterion":"AC-001","passed":true,"evidence":["cargo test"]}],
        "verification":["cargo test"]
    });
    store
        .add_report(
            "impl-review",
            &task.task_id,
            ReportKind::Review,
            1,
            0,
            implementation_review,
        )
        .unwrap();
    store
        .add_report(
            "impl-harness",
            &task.task_id,
            ReportKind::Harness,
            1,
            0,
            json!({"spec_version":1,"round":0,"stage":"IMPLEMENTATION","findings":[]}),
        )
        .unwrap();
    apply(
        EventType::ReviewCompleted,
        "impl-decision",
        json!({"verdict":"APPROVED","stage":"IMPLEMENTATION"}),
    );
    apply(
        EventType::IntegrationPrepared,
        "integration-prepared",
        json!({"repositories":[{"repository_id":"repo-code","tree_changed":true}]}),
    );
    let current = store.get_task(&task.task_id).unwrap().unwrap();
    assert_eq!(current.phase, Phase::FinalReview);

    // Only a new FINAL_DELIVERY review exists; the implementation harness must
    // not stand in for the final harness.
    store
        .add_report(
            "final-review",
            &task.task_id,
            ReportKind::Review,
            1,
            0,
            json!({
                "verdict":"APPROVED","spec_version":1,"round":0,"stage":"FINAL_DELIVERY",
                "reviewed_versions":[{"repository_id":"repo-code","commit":"integrated"}],
                "acceptance_results":[{"criterion":"AC-001","passed":true,"evidence":["cargo test"]}],
                "verification":["cargo test"]
            }),
        )
        .unwrap();
    let mut report = TickReport::default();
    orchestrator
        .decide_review(&store, &current, &mut report)
        .unwrap();
    assert!(
        !store.events(&task.task_id).unwrap().iter().any(|event| {
            event.event_type == EventType::ReviewCompleted
                && event.payload["stage"].as_str() == Some("FINAL_DELIVERY")
        }),
        "the implementation harness must not decide the final review"
    );
    assert_eq!(
        store.get_task(&task.task_id).unwrap().unwrap().phase,
        Phase::FinalReview
    );

    // The new final harness completes the evidence and the host decides.
    store
        .add_report(
            "final-harness",
            &task.task_id,
            ReportKind::Harness,
            1,
            0,
            json!({"spec_version":1,"round":0,"stage":"FINAL_DELIVERY","findings":[]}),
        )
        .unwrap();
    orchestrator
        .decide_review(&store, &current, &mut report)
        .unwrap();
    assert!(
        store.events(&task.task_id).unwrap().iter().any(|event| {
            event.event_type == EventType::ReviewCompleted
                && event.payload["stage"].as_str() == Some("FINAL_DELIVERY")
        }),
        "the final review was not decided after both final reports"
    );
    // Approval of integrated content is not delivery.
    assert_eq!(
        store.get_task(&task.task_id).unwrap().unwrap().phase,
        Phase::DeliveryPending
    );
}

/// Build in-memory delivery identities only: these tests never launch a child
/// and never touch Git.
fn delivery_identities() -> (
    Fixture,
    TaskRecord,
    crate::super_agent::delivery::DeliveryPlan,
    crate::super_agent::delivery::DeliveryDispatch,
) {
    let fixture = Fixture::new();
    let mut task = fixture.task();
    task.phase = Phase::DeliveryPending;
    task.status = Status::Running;
    task.review_round = 1;
    task.coder.session_id = Some("coder-session".into());
    let plan = crate::super_agent::delivery::DeliveryPlan {
        operation_id: "plan-1".into(),
        task_id: task.task_id.clone(),
        spec_version: task.spec_version,
        review_round: task.review_round,
        evidence_event_id: "integration-event".into(),
        review_event_id: "review-event".into(),
        content_fingerprint: "fingerprint".into(),
        created_at: "2026-01-01T00:00:00Z".into(),
        repositories: vec![crate::super_agent::delivery::DeliveryTargetPlan {
            repository_id: "repo-1".into(),
            repo_root: PathBuf::from("/repo"),
            git_common_dir: PathBuf::from("/repo/.git"),
            target_branch: "develop".into(),
            target_ref: "refs/heads/develop".into(),
            destination: crate::super_agent::delivery::DeliveryDestination::RefOnly,
            baseline_commit: "old".into(),
            reviewed_commit: "reviewed".into(),
            integrated_commit: "new".into(),
            integrated_tree: "tree".into(),
            old_oid: "old".into(),
            new_oid: "new".into(),
            review_stage: crate::super_agent::model::ReviewStage::Implementation,
            lock_key: "branch:/repo/.git:refs/heads/develop".into(),
            lock_operation_id: "lock-op".into(),
            attempt: 1,
        }],
    };
    let identity = crate::super_agent::delivery::DeliveryOperationIdentity::for_target(
        &plan,
        &plan.repositories[0],
        "coder-session",
    );
    let dispatch = crate::super_agent::delivery::DeliveryDispatch {
        task_id: task.task_id.clone(),
        spec_version: task.spec_version,
        review_round: task.review_round,
        plan_operation_id: plan.operation_id.clone(),
        coder_session_id: "coder-session".into(),
        purpose: identity.purpose_key(),
        repository_id: Some("repo-1".into()),
        old_oid: Some("old".into()),
        new_oid: Some("new".into()),
        attempt: 1,
        job_id: None,
        created_at: "2026-01-01T00:00:00Z".into(),
    };
    (fixture, task, plan, dispatch)
}

fn delivery_operation(
    task: &TaskRecord,
    plan: &crate::super_agent::delivery::DeliveryPlan,
    dispatch: &crate::super_agent::delivery::DeliveryDispatch,
) -> crate::super_agent::launcher::LaunchOperation {
    let identity = crate::super_agent::delivery::DeliveryOperationIdentity::for_target(
        plan,
        &plan.repositories[0],
        &dispatch.coder_session_id,
    );
    crate::super_agent::launcher::LaunchOperation {
        operation_id: format!("{}:coder:resume", task.task_id),
        task_id: task.task_id.clone(),
        role: "coder".into(),
        kind: "resume".into(),
        spec_version: task.spec_version,
        review_round: task.review_round,
        purpose: identity.purpose_key(),
        delivery: Some(identity),
        workspace: task.spec.workspace.clone(),
        session_id: dispatch.coder_session_id.clone(),
        job_id: "job-1".into(),
        resume_session: Some(dispatch.coder_session_id.clone()),
        prompt: "deliver".into(),
        model: "fixture-model".into(),
        permission_mode: "bypass".into(),
        warning: None,
        allow_workspace_move: false,
        state: "reserved".into(),
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
        last_job_status: None,
        last_error: None,
        consumed_at: None,
        handle: None,
    }
}

fn legacy_delivery_operation(
    task: &TaskRecord,
    dispatch: &crate::super_agent::delivery::DeliveryDispatch,
) -> crate::super_agent::launcher::LaunchOperation {
    crate::super_agent::launcher::LaunchOperation {
        operation_id: format!("{}:coder:resume", task.task_id),
        task_id: task.task_id.clone(),
        role: "coder".into(),
        kind: "resume".into(),
        spec_version: task.spec_version,
        review_round: task.review_round,
        purpose: crate::super_agent::delivery::LEGACY_DELIVERY_PURPOSE.into(),
        delivery: None,
        workspace: task.spec.workspace.clone(),
        session_id: dispatch.coder_session_id.clone(),
        job_id: "job-1".into(),
        resume_session: Some(dispatch.coder_session_id.clone()),
        prompt: "deliver".into(),
        model: "fixture-model".into(),
        permission_mode: "bypass".into(),
        warning: None,
        allow_workspace_move: false,
        state: "completed".into(),
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
        last_job_status: None,
        last_error: None,
        consumed_at: None,
        handle: None,
    }
}

fn delivery_handle(
    operation: &crate::super_agent::launcher::LaunchOperation,
) -> crate::super_agent::launcher::LaunchRecord {
    crate::super_agent::launcher::LaunchRecord {
        role: "coder".into(),
        session_id: operation.session_id.clone(),
        job_id: operation.job_id.clone(),
        workspace: operation.workspace.clone(),
        model: operation.model.clone(),
        delivery: operation.delivery.clone(),
        warning: None,
        created_at: "2026-01-01T00:00:00Z".into(),
    }
}

fn delivery_job(
    task: &TaskRecord,
    operation: &crate::super_agent::launcher::LaunchOperation,
) -> crate::services::background_sessions::Job {
    crate::services::background_sessions::Job {
        id: operation.job_id.clone(),
        foreground: false,
        parent_session_id: task.router_session_id.clone(),
        session_id: operation.session_id.clone(),
        title: "delivery".into(),
        status: "running".into(),
        supervisor_pid: None,
        child_pid: None,
        started_ms: 0,
        heartbeat_ms: 0,
        finished_ms: None,
        exit_code: None,
        error: None,
        launched: true,
    }
}

#[test]
fn a_matching_delivery_identity_is_accepted() {
    let (_fixture, task, plan, dispatch) = delivery_identities();
    let operation = delivery_operation(&task, &plan, &dispatch);
    let handle = delivery_handle(&operation);
    let job = delivery_job(&task, &operation);
    assert_eq!(delivery_context_mismatch(&task, &plan, &dispatch), None);
    assert_eq!(
        delivery_operation_mismatch(&task, &plan, &dispatch, &operation),
        None
    );
    assert_eq!(
        delivery_handle_mismatch(&operation, &dispatch, &handle),
        None
    );
    assert_eq!(
        delivery_job_identity_mismatch(&task, &operation.job_id, &operation.session_id, &job),
        None
    );
}

#[test]
fn a_contradictory_receipt_or_operation_is_rejected() {
    let (_fixture, task, plan, dispatch) = delivery_identities();
    let operation = delivery_operation(&task, &plan, &dispatch);

    let rejected = |plan: &crate::super_agent::delivery::DeliveryPlan,
                    dispatch: &crate::super_agent::delivery::DeliveryDispatch| {
        delivery_context_mismatch(&task, plan, dispatch)
            .expect("a contradictory receipt must be rejected")
    };

    let mut changed = plan.clone();
    changed.task_id = "other-task".into();
    assert!(rejected(&changed, &dispatch).contains("other-task"));

    let mut changed = plan.clone();
    changed.spec_version += 1;
    assert!(rejected(&changed, &dispatch).contains("spec"));

    let mut changed = plan.clone();
    changed.review_round += 1;
    assert!(rejected(&changed, &dispatch).contains("round"));

    let mut changed = dispatch.clone();
    changed.task_id = "other-task".into();
    assert!(rejected(&plan, &changed).contains("other-task"));

    let mut changed = dispatch.clone();
    changed.spec_version += 1;
    assert!(rejected(&plan, &changed).contains("spec"));

    let mut changed = dispatch.clone();
    changed.review_round += 1;
    assert!(rejected(&plan, &changed).contains("round"));

    let mut changed = dispatch.clone();
    changed.plan_operation_id = "other-plan".into();
    assert!(rejected(&plan, &changed).contains("other-plan"));

    let mut changed = dispatch.clone();
    changed.purpose = "implementation".into();
    assert!(rejected(&plan, &changed).contains("implementation"));

    let mut changed = dispatch.clone();
    changed.coder_session_id = "other-coder".into();
    assert!(rejected(&plan, &changed).contains("other-coder"));

    // The receipt must name a real repository of the complete plan and the
    // approved old/new OIDs and attempt.
    let mut changed = dispatch.clone();
    changed.repository_id = None;
    assert!(rejected(&plan, &changed).contains("does not name the repository"));

    let mut changed = dispatch.clone();
    changed.repository_id = Some("repo-2".into());
    assert!(rejected(&plan, &changed).contains("repo-2"));

    let mut changed = dispatch.clone();
    changed.old_oid = Some("other-old".into());
    assert!(rejected(&plan, &changed).contains("approved old/new OIDs"));

    let mut changed = dispatch.clone();
    changed.new_oid = Some("other-new".into());
    assert!(rejected(&plan, &changed).contains("approved old/new OIDs"));

    let mut changed = dispatch.clone();
    changed.attempt += 1;
    assert!(rejected(&plan, &changed).contains("approved old/new OIDs"));

    // The operation is found under the right key but its contents belong to
    // another task, evidence stage or session.
    let operation_rejected = |operation: &crate::super_agent::launcher::LaunchOperation| {
        delivery_operation_mismatch(&task, &plan, &dispatch, operation)
            .expect("a contradictory operation must be rejected")
    };

    let mut changed = operation.clone();
    changed.task_id = "other-task".into();
    assert!(operation_rejected(&changed).contains("other-task"));

    let mut changed = operation.clone();
    changed.role = "reviewer".into();
    assert!(operation_rejected(&changed).contains("reviewer"));

    let mut changed = operation.clone();
    changed.kind = "initial".into();
    assert!(operation_rejected(&changed).contains("initial"));

    let mut changed = operation.clone();
    changed.spec_version += 1;
    assert!(operation_rejected(&changed).contains("spec"));

    let mut changed = operation.clone();
    changed.review_round += 1;
    assert!(operation_rejected(&changed).contains("round"));

    let mut changed = operation.clone();
    changed.purpose = "implementation".into();
    assert!(operation_rejected(&changed).contains("implementation"));

    let mut changed = operation.clone();
    changed.session_id = "other-coder".into();
    assert!(operation_rejected(&changed).contains("other-coder"));

    let mut changed = operation.clone();
    changed.resume_session = Some("other-coder".into());
    assert!(operation_rejected(&changed).contains("other-coder"));

    // A contradictory plan is also rejected even when the operation itself is
    // internally consistent.
    let mut changed_plan = plan.clone();
    changed_plan.task_id = "other-task".into();
    assert!(
        delivery_operation_mismatch(&task, &changed_plan, &dispatch, &operation)
            .expect("a contradictory plan must be rejected")
            .contains("other-task")
    );

    // Every typed identity fact is part of the adoption decision. Editing the
    // stored identity under the same key is a contradiction, not a new launch.
    let base = operation.delivery.clone().expect("typed identity");
    for mutate in [
        |identity: &mut crate::super_agent::delivery::DeliveryOperationIdentity| {
            identity.repository_id = "repo-2".into()
        },
        |identity: &mut crate::super_agent::delivery::DeliveryOperationIdentity| {
            identity.plan_operation_id = "other-plan".into()
        },
        |identity: &mut crate::super_agent::delivery::DeliveryOperationIdentity| {
            identity.evidence_event_id = "other-evidence".into()
        },
        |identity: &mut crate::super_agent::delivery::DeliveryOperationIdentity| {
            identity.review_event_id = "other-review".into()
        },
        |identity: &mut crate::super_agent::delivery::DeliveryOperationIdentity| {
            identity.content_fingerprint = "other-fingerprint".into()
        },
        |identity: &mut crate::super_agent::delivery::DeliveryOperationIdentity| {
            identity.old_oid = "other-old".into()
        },
        |identity: &mut crate::super_agent::delivery::DeliveryOperationIdentity| {
            identity.new_oid = "other-new".into()
        },
        |identity: &mut crate::super_agent::delivery::DeliveryOperationIdentity| {
            identity.attempt += 1
        },
        |identity: &mut crate::super_agent::delivery::DeliveryOperationIdentity| {
            identity.coder_session_id = "other-coder".into()
        },
        |identity: &mut crate::super_agent::delivery::DeliveryOperationIdentity| {
            identity.version += 1
        },
    ] {
        let mut changed = operation.clone();
        let mut identity = base.clone();
        mutate(&mut identity);
        changed.delivery = Some(identity);
        assert!(
            delivery_operation_mismatch(&task, &plan, &dispatch, &changed).is_some(),
            "a contradictory typed delivery identity must be rejected"
        );
    }
    // A typed record whose identity field was removed is incomplete, not a
    // genuine legacy record.
    let mut changed = operation.clone();
    changed.delivery = None;
    assert!(delivery_operation_mismatch(&task, &plan, &dispatch, &changed)
        .expect("an incomplete typed record must be rejected")
        .contains("purpose"));
}

#[test]
fn a_genuine_legacy_record_is_adopted_only_through_the_unique_plan() {
    let (_fixture, task, plan, typed_dispatch) = delivery_identities();
    let legacy_dispatch = crate::super_agent::delivery::DeliveryDispatch {
        purpose: crate::super_agent::delivery::LEGACY_DELIVERY_PURPOSE.into(),
        ..typed_dispatch.clone()
    };
    let legacy = legacy_delivery_operation(&task, &legacy_dispatch);
    // A legacy receipt, a single-repository persisted plan and a genuinely
    // legacy operation (no typed field) are accepted together.
    assert_eq!(delivery_context_mismatch(&task, &plan, &legacy_dispatch), None);
    assert_eq!(
        delivery_operation_mismatch(&task, &plan, &legacy_dispatch, &legacy),
        None
    );
    let handle = delivery_handle(&legacy);
    assert_eq!(
        delivery_handle_mismatch(&legacy, &legacy_dispatch, &handle),
        None
    );

    // A multi-repository plan cannot be bound by an ambiguous old receipt.
    let mut multi = plan.clone();
    let mut second = plan.repositories[0].clone();
    second.repository_id = "repo-2".into();
    multi.repositories.push(second);
    assert!(delivery_operation_mismatch(&task, &multi, &legacy_dispatch, &legacy)
        .expect("a multi-repository legacy record must be rejected")
        .contains("legacy"));

    // A typed operation never satisfies a legacy receipt, and a legacy
    // operation never satisfies a typed receipt: a rollback cannot shed the
    // required identity proof.
    let typed = delivery_operation(&task, &plan, &typed_dispatch);
    assert!(delivery_operation_mismatch(&task, &plan, &legacy_dispatch, &typed).is_some());
    assert!(delivery_operation_mismatch(&task, &plan, &typed_dispatch, &legacy).is_some());

    // A record created by the new code always carries the typed purpose and
    // identity, so it cannot be mistaken for a legacy one merely because one
    // field is missing; the launcher reads it only from the typed key.
    let mut incomplete = typed.clone();
    incomplete.delivery = None;
    assert!(delivery_operation_mismatch(&task, &plan, &typed_dispatch, &incomplete).is_some());
}

#[test]
fn a_handle_or_actual_job_that_points_elsewhere_is_rejected() {
    let (_fixture, task, plan, dispatch) = delivery_identities();
    let operation = delivery_operation(&task, &plan, &dispatch);
    let handle = delivery_handle(&operation);
    let job = delivery_job(&task, &operation);

    let mut changed = handle.clone();
    changed.role = "reviewer".into();
    assert!(delivery_handle_mismatch(&operation, &dispatch, &changed)
        .expect("role mismatch")
        .contains("reviewer"));

    let mut changed = handle.clone();
    changed.session_id = "other-coder".into();
    assert!(delivery_handle_mismatch(&operation, &dispatch, &changed)
        .expect("session mismatch")
        .contains("other-coder"));

    let mut changed = handle.clone();
    changed.job_id = "other-job".into();
    assert!(delivery_handle_mismatch(&operation, &dispatch, &changed)
        .expect("job mismatch")
        .contains("other-job"));

    let mut changed = job.clone();
    changed.id = "other-job".into();
    assert!(delivery_job_identity_mismatch(
        &task,
        &operation.job_id,
        &operation.session_id,
        &changed
    )
    .expect("job id mismatch")
    .contains("other-job"));

    let mut changed = job.clone();
    changed.session_id = "other-coder".into();
    assert!(delivery_job_identity_mismatch(
        &task,
        &operation.job_id,
        &operation.session_id,
        &changed
    )
    .expect("job session mismatch")
    .contains("other-coder"));

    let mut changed = job;
    changed.parent_session_id = "other-router".into();
    assert!(delivery_job_identity_mismatch(
        &task,
        &operation.job_id,
        &operation.session_id,
        &changed
    )
    .expect("job Router mismatch")
    .contains("other-router"));
}

/// A malformed reserved job ID or an inspection error must block explicitly
/// instead of escaping the tick, and must never bind a receipt or launch.
#[test]
fn a_malformed_or_unreadable_delivery_job_blocks_without_binding() {
    let fixture = Fixture::new();
    let store = fixture.store();
    let orchestrator = fixture.orchestrator(json!({}));
    let mut task = fixture.task();
    task.coder.session_id = Some("coder-session".into());

    let mut report = TickReport::default();
    let check = orchestrator
        .check_delivery_job(&store, &task, "not-a-job-id", "coder-session", &mut report)
        .unwrap();
    match check {
        DeliveryJobCheck::Blocked(blocked) => {
            assert_eq!(blocked.status, Status::Blocked);
            assert!(
                report
                    .blocked
                    .iter()
                    .any(|line| line.contains("cannot be read")),
                "{report:?}"
            );
        }
        DeliveryJobCheck::Valid(_) => panic!("a malformed job ID must block"),
    }
    let blocked_event = store
        .events(&task.task_id)
        .unwrap()
        .into_iter()
        .find(|event| event.event_type == EventType::TaskBlocked)
        .expect("explicit TaskBlocked event");
    assert!(
        blocked_event.payload["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("cannot be read")),
        "{:?}",
        blocked_event.payload
    );

    // An unreadable state file is blocked too, and its bytes are preserved.
    let job_id = uuid::Uuid::new_v4().to_string();
    let directory = fixture.data.join("background-tasks").join(&job_id);
    std::fs::create_dir_all(&directory).unwrap();
    let state = directory.join("state.json");
    let broken = b"{not-json";
    std::fs::write(&state, broken).unwrap();
    let mut unreadable_task = store
        .create_task(&fixture.router, fixture.spec(), "unreadable-job-test")
        .unwrap();
    unreadable_task.coder.session_id = Some("coder-session".into());
    let mut unreadable_report = TickReport::default();
    let check = orchestrator
        .check_delivery_job(
            &store,
            &unreadable_task,
            &job_id,
            "coder-session",
            &mut unreadable_report,
        )
        .unwrap();
    match check {
        DeliveryJobCheck::Blocked(blocked) => {
            assert_eq!(blocked.status, Status::Blocked);
            assert!(
                unreadable_report
                    .blocked
                    .iter()
                    .any(|line| line.contains("unreadable state")),
                "{unreadable_report:?}"
            );
        }
        DeliveryJobCheck::Valid(_) => panic!("an unreadable job must block"),
    }
    assert_eq!(std::fs::read(&state).unwrap(), broken);
}

fn repository_plan(task: &TaskRecord, repositories: &[(&str, &str, &str)]) -> DeliveryPlan {
    DeliveryPlan {
        operation_id: "bound-plan".into(),
        task_id: task.task_id.clone(),
        spec_version: task.spec_version,
        review_round: task.review_round,
        evidence_event_id: "evidence".into(),
        review_event_id: "review".into(),
        content_fingerprint: "fingerprint".into(),
        created_at: "2026-01-01T00:00:00Z".into(),
        repositories: repositories
            .iter()
            .enumerate()
            .map(|(index, (repository_id, old_oid, new_oid))| DeliveryTargetPlan {
                repository_id: (*repository_id).into(),
                repo_root: PathBuf::from(format!("/fixture/{repository_id}")),
                git_common_dir: PathBuf::from(format!("/fixture/{repository_id}/.git")),
                target_branch: "develop".into(),
                target_ref: "refs/heads/develop".into(),
                destination: DeliveryDestination::RefOnly,
                baseline_commit: (*old_oid).into(),
                reviewed_commit: (*new_oid).into(),
                integrated_commit: (*new_oid).into(),
                integrated_tree: "tree".into(),
                old_oid: (*old_oid).into(),
                new_oid: (*new_oid).into(),
                review_stage: crate::super_agent::model::ReviewStage::Implementation,
                lock_key: format!("branch:/fixture/{repository_id}/.git:refs/heads/develop"),
                lock_operation_id: format!("lock-{index}"),
                attempt: 1,
            })
            .collect(),
    }
}

fn typed_operation(
    launcher: &crate::super_agent::launcher::Launcher,
    task: &TaskRecord,
    plan: &DeliveryPlan,
    target: &DeliveryTargetPlan,
    job_id: &str,
) -> crate::super_agent::launcher::LaunchOperation {
    let identity = DeliveryOperationIdentity::for_target(plan, target, "coder-session");
    let operation = crate::super_agent::launcher::LaunchOperation {
        operation_id: format!("{}:coder:resume", task.task_id),
        task_id: task.task_id.clone(),
        role: "coder".into(),
        kind: "resume".into(),
        spec_version: task.spec_version,
        review_round: task.review_round,
        purpose: identity.purpose_key(),
        delivery: Some(identity.clone()),
        workspace: task.spec.workspace.clone(),
        session_id: "coder-session".into(),
        job_id: job_id.into(),
        resume_session: Some("coder-session".into()),
        prompt: "deliver".into(),
        model: "fixture-model".into(),
        permission_mode: "bypass".into(),
        warning: None,
        allow_workspace_move: false,
        state: "completed".into(),
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
        last_job_status: None,
        last_error: None,
        consumed_at: None,
        handle: Some(crate::super_agent::launcher::LaunchRecord {
            role: "coder".into(),
            session_id: "coder-session".into(),
            job_id: job_id.into(),
            workspace: task.spec.workspace.clone(),
            model: "fixture-model".into(),
            delivery: Some(identity),
            warning: None,
            created_at: "2026-01-01T00:00:00Z".into(),
        }),
    };
    let key = launcher.operation_key_for(
        task,
        crate::super_agent::MemberRole::Coder,
        LaunchKind::Resume,
        &operation.purpose,
    );
    launcher.context.save_setting(&key, &operation).unwrap();
    operation
}

fn legacy_operation(
    launcher: &crate::super_agent::launcher::Launcher,
    task: &TaskRecord,
    job_id: &str,
) -> crate::super_agent::launcher::LaunchOperation {
    let operation = crate::super_agent::launcher::LaunchOperation {
        operation_id: format!("{}:coder:resume", task.task_id),
        task_id: task.task_id.clone(),
        role: "coder".into(),
        kind: "resume".into(),
        spec_version: task.spec_version,
        review_round: task.review_round,
        purpose: "delivery".into(),
        delivery: None,
        workspace: task.spec.workspace.clone(),
        session_id: "coder-session".into(),
        job_id: job_id.into(),
        resume_session: Some("coder-session".into()),
        prompt: "deliver".into(),
        model: "fixture-model".into(),
        permission_mode: "bypass".into(),
        warning: None,
        allow_workspace_move: false,
        state: "completed".into(),
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
        last_job_status: None,
        last_error: None,
        consumed_at: None,
        handle: None,
    };
    let key = launcher.operation_key_for(
        task,
        crate::super_agent::MemberRole::Coder,
        LaunchKind::Resume,
        "delivery",
    );
    launcher.context.save_setting(&key, &operation).unwrap();
    operation
}

fn binding_path(data: &Path, job_id: &str) -> PathBuf {
    data.join("background-tasks")
        .join(job_id)
        .join("delivery-identity.json")
}

fn write_binding(data: &Path, job_id: &str, identity: &DeliveryOperationIdentity) {
    let path = binding_path(data, job_id);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        serde_json::to_vec(&DeliveryJobBinding {
            version: 1,
            job_id: job_id.to_owned(),
            session_id: identity.coder_session_id.clone(),
            identity: identity.clone(),
        })
        .unwrap(),
    )
    .unwrap();
}

fn bound_dispatch(
    plan: &DeliveryPlan,
    target: &DeliveryTargetPlan,
    identity: &DeliveryOperationIdentity,
    job_id: &str,
) -> DeliveryDispatch {
    DeliveryDispatch {
        task_id: plan.task_id.clone(),
        spec_version: plan.spec_version,
        review_round: plan.review_round,
        plan_operation_id: plan.operation_id.clone(),
        coder_session_id: identity.coder_session_id.clone(),
        purpose: identity.purpose_key(),
        repository_id: Some(target.repository_id.clone()),
        old_oid: Some(target.old_oid.clone()),
        new_oid: Some(target.new_oid.clone()),
        attempt: target.attempt,
        job_id: Some(job_id.to_owned()),
        created_at: "2026-01-01T00:00:00Z".into(),
    }
}

fn delivery_task(fixture: &Fixture) -> TaskRecord {
    let mut task = fixture.task();
    task.phase = Phase::DeliveryPending;
    task.status = Status::Running;
    task.review_round = 1;
    task.coder.session_id = Some("coder-session".into());
    task
}

#[test]
fn a_bound_receipt_must_name_its_own_job_operation_and_durable_identity() {
    let fixture = Fixture::new();
    let task = delivery_task(&fixture);
    let plan = repository_plan(
        &task,
        &[("repo-1", "a-old", "a-new"), ("repo-2", "b-old", "b-new")],
    );
    let launcher = fixture.launcher();
    let orchestrator = fixture.orchestrator(json!({"agents":{}}));
    let identity_a = DeliveryOperationIdentity::for_target(&plan, &plan.repositories[0], "coder-session");
    let identity_b = DeliveryOperationIdentity::for_target(&plan, &plan.repositories[1], "coder-session");
    let job_a = uuid::Uuid::new_v4().to_string();
    let job_b = uuid::Uuid::new_v4().to_string();
    let operation_a = typed_operation(&launcher, &task, &plan, &plan.repositories[0], &job_a);
    typed_operation(&launcher, &task, &plan, &plan.repositories[1], &job_b);
    write_binding(&fixture.data, &job_a, &identity_a);
    write_binding(&fixture.data, &job_b, &identity_b);
    let dispatch_a = bound_dispatch(&plan, &plan.repositories[0], &identity_a, &job_a);
    let dispatch_b = bound_dispatch(&plan, &plan.repositories[1], &identity_b, &job_b);

    // Each repository's own receipt and job are accepted.
    assert_eq!(
        orchestrator
            .bound_delivery_mismatch(&task, &plan, &dispatch_a, &job_a)
            .unwrap(),
        None
    );
    assert_eq!(
        orchestrator
            .bound_delivery_mismatch(&task, &plan, &dispatch_b, &job_b)
            .unwrap(),
        None
    );

    // A receipt that names repository A but the job reserved for repository B
    // shares the same Coder and Router, and is still refused.
    let reason = orchestrator
        .bound_delivery_mismatch(&task, &plan, &dispatch_a, &job_b)
        .unwrap()
        .expect("a swapped job must be refused");
    assert!(
        reason.contains("reserved") && reason.contains(&job_a),
        "{reason}"
    );

    let key_a = launcher.operation_key_for(
        &task,
        crate::super_agent::MemberRole::Coder,
        LaunchKind::Resume,
        &identity_a.purpose_key(),
    );
    let stored: Value = launcher.context.setting(&key_a).unwrap().unwrap();
    let stored_binding = std::fs::read(binding_path(&fixture.data, &job_a)).unwrap();

    // A bound receipt whose operation record is missing has no proof.
    launcher
        .context
        .db
        .with_conn(|conn| {
            conn.execute(
                "DELETE FROM peripheral_settings WHERE key=?1",
                [&key_a],
            )
        })
        .unwrap()
        .unwrap();
    let reason = orchestrator
        .bound_delivery_mismatch(&task, &plan, &dispatch_a, &job_a)
        .unwrap()
        .expect("a receipt without its operation must be refused");
    assert!(reason.contains("no launch operation"), "{reason}");
    launcher.context.save_setting(&key_a, &stored).unwrap();

    // A typed operation whose identity field was removed is incomplete, not
    // legacy, and cannot be observed.
    let mut stripped = serde_json::to_value(&operation_a).unwrap();
    stripped.as_object_mut().unwrap().remove("delivery");
    launcher.context.save_setting(&key_a, &stripped).unwrap();
    let reason = orchestrator
        .bound_delivery_mismatch(&task, &plan, &dispatch_a, &job_a)
        .unwrap()
        .expect("a typed operation without its identity must be refused");
    assert!(
        reason.contains("does not match the receipt") && reason.contains("purpose"),
        "{reason}"
    );
    launcher.context.save_setting(&key_a, &stored).unwrap();

    // Changing the operation's repository, plan, evidence, OIDs, attempt or
    // session is a contradiction, and the stored row is preserved.
    for mutate in [
        |value: &mut Value| value["delivery"]["repository_id"] = json!("repo-2"),
        |value: &mut Value| value["delivery"]["plan_operation_id"] = json!("other-plan"),
        |value: &mut Value| value["delivery"]["evidence_event_id"] = json!("other-evidence"),
        |value: &mut Value| value["delivery"]["old_oid"] = json!("other-old"),
        |value: &mut Value| value["delivery"]["new_oid"] = json!("other-new"),
        |value: &mut Value| value["delivery"]["attempt"] = json!(2),
        |value: &mut Value| value["delivery"]["coder_session_id"] = json!("other-coder"),
    ] {
        let mut changed = stored.clone();
        mutate(&mut changed);
        launcher.context.save_setting(&key_a, &changed).unwrap();
        let reason = orchestrator
            .bound_delivery_mismatch(&task, &plan, &dispatch_a, &job_a)
            .unwrap()
            .expect("a contradictory operation must be refused");
        assert!(!reason.is_empty());
        launcher.context.save_setting(&key_a, &stored).unwrap();
    }

    // The job's own durable binding must match the operation, not only the
    // Coder session.
    std::fs::remove_file(binding_path(&fixture.data, &job_a)).unwrap();
    let reason = orchestrator
        .bound_delivery_mismatch(&task, &plan, &dispatch_a, &job_a)
        .unwrap()
        .expect("a typed job without its binding must be refused");
    assert!(reason.contains("no durable delivery identity"), "{reason}");
    write_binding(&fixture.data, &job_a, &identity_b);
    let reason = orchestrator
        .bound_delivery_mismatch(&task, &plan, &dispatch_a, &job_a)
        .unwrap()
        .expect("a job bound to another identity must be refused");
    assert!(reason.contains("carries delivery identity"), "{reason}");
    std::fs::write(binding_path(&fixture.data, &job_a), &stored_binding).unwrap();

    // The handle must point at the same reserved job as the operation.
    let mut moved = stored.clone();
    moved["job_id"] = json!(job_b);
    launcher.context.save_setting(&key_a, &moved).unwrap();
    let reason = orchestrator
        .bound_delivery_mismatch(&task, &plan, &dispatch_a, &job_b)
        .unwrap()
        .expect("a contradictory handle must be refused");
    assert!(reason.contains("recorded delivery child"), "{reason}");

    // Everything rejected above left the original rows byte-identical.
    assert_eq!(
        launcher.context.setting::<Value>(&key_a).unwrap().unwrap()["task_id"],
        json!(task.task_id)
    );
    assert_eq!(
        std::fs::read(binding_path(&fixture.data, &job_a)).unwrap(),
        stored_binding
    );
}

#[test]
fn a_bound_legacy_receipt_is_only_adopted_through_its_genuine_job() {
    let fixture = Fixture::new();
    let task = delivery_task(&fixture);
    let plan = repository_plan(&task, &[("repo-1", "old", "new")]);
    let launcher = fixture.launcher();
    let orchestrator = fixture.orchestrator(json!({"agents":{}}));
    let identity = DeliveryOperationIdentity::for_target(&plan, &plan.repositories[0], "coder-session");
    let legacy_job = uuid::Uuid::new_v4().to_string();
    let typed_job = uuid::Uuid::new_v4().to_string();
    legacy_operation(&launcher, &task, &legacy_job);
    let legacy_dispatch = DeliveryDispatch {
        purpose: "delivery".into(),
        ..bound_dispatch(&plan, &plan.repositories[0], &identity, &legacy_job)
    };

    // A genuine legacy operation, a legacy receipt uniquely bound by the
    // single-repository plan and an existing job without any typed binding
    // are adopted together.
    assert_eq!(
        orchestrator
            .bound_delivery_mismatch(&task, &plan, &legacy_dispatch, &legacy_job)
            .unwrap(),
        None
    );

    // A legacy receipt cannot be pointed at a job that carries a typed
    // identity, and a typed receipt cannot reuse the legacy operation.
    write_binding(&fixture.data, &legacy_job, &identity);
    let reason = orchestrator
        .bound_delivery_mismatch(&task, &plan, &legacy_dispatch, &legacy_job)
        .unwrap()
        .expect("a legacy receipt cannot adopt a typed job");
    assert!(reason.contains("mix identities"), "{reason}");
    std::fs::remove_file(binding_path(&fixture.data, &legacy_job)).unwrap();

    let typed_dispatch = bound_dispatch(&plan, &plan.repositories[0], &identity, &typed_job);
    let reason = orchestrator
        .bound_delivery_mismatch(&task, &plan, &typed_dispatch, &legacy_job)
        .unwrap()
        .expect("a typed receipt cannot adopt the legacy operation");
    assert!(
        reason.contains("no launch operation") || reason.contains("does not match the receipt"),
        "{reason}"
    );
}
