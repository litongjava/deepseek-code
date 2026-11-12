use super::*;
use crate::native_engine::provider_manager::ResolvedProvider;
use crate::runtime::{RuntimePaths, SessionStore};
use crate::super_agent::config::SuperAgentConfig;
use crate::super_agent::crp::{self, ToolRuntime};
use crate::super_agent::delivery::{
    DeliveryDestination, DeliveryDispatch, DeliveryOperationIdentity, DeliveryPlan,
    DeliveryTargetPlan,
};
use crate::super_agent::launcher::DeliveryJobBinding;
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
    assert!(
        delivery_operation_mismatch(&task, &plan, &dispatch, &changed)
            .expect("an incomplete typed record must be rejected")
            .contains("purpose")
    );
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
    assert_eq!(
        delivery_context_mismatch(&task, &plan, &legacy_dispatch),
        None
    );
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
    assert!(
        delivery_operation_mismatch(&task, &multi, &legacy_dispatch, &legacy)
            .expect("a multi-repository legacy record must be rejected")
            .contains("legacy")
    );

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
            .map(
                |(index, (repository_id, old_oid, new_oid))| DeliveryTargetPlan {
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
                },
            )
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
    let identity_a =
        DeliveryOperationIdentity::for_target(&plan, &plan.repositories[0], "coder-session");
    let identity_b =
        DeliveryOperationIdentity::for_target(&plan, &plan.repositories[1], "coder-session");
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
        .with_conn(|conn| conn.execute("DELETE FROM peripheral_settings WHERE key=?1", [&key_a]))
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
    let identity =
        DeliveryOperationIdentity::for_target(&plan, &plan.repositories[0], "coder-session");
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

#[cfg(unix)]
mod two_repository_delivery {
    use super::*;

    /// Two independent real Git repositories whose approved integration is already
    /// recorded. Delivery runs through the real launcher using a supervisor
    /// script: it executes the production worker entry for the real job and a
    /// no-op child for the launched command, so these tests exercise the
    /// sequential per-repository state machine without a model.
    struct DeliveryRepoFixture {
        repository_id: String,
        root: PathBuf,
        worktree: PathBuf,
        branch: String,
        base: String,
        reviewed: String,
    }

    struct TwoRepoDeliveryFixture {
        _temp: tempfile::TempDir,
        data: PathBuf,
        context: ServiceContext,
        router: String,
        task_id: String,
        repos: Vec<DeliveryRepoFixture>,
        supervisor: PathBuf,
    }

    /// Owns the PIDs of a held fixture child. Cleanup uses the PIDs the script
    /// recorded itself, never the durable job state, so it still works after a
    /// test deliberately damages that state.
    struct HeldChildren {
        pids: PathBuf,
        release: PathBuf,
        watched: Vec<u32>,
    }

    impl HeldChildren {
        fn wait_for_pids(&mut self) -> (u32, u32) {
            for _ in 0..1200 {
                if let Ok(text) = std::fs::read_to_string(&self.pids) {
                    let mut lines = text
                        .lines()
                        .filter_map(|line| line.trim().parse::<u32>().ok());
                    if let (Some(child), Some(supervisor)) = (lines.next(), lines.next()) {
                        self.watched = vec![child, supervisor];
                        return (child, supervisor);
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            panic!("the held child did not record its process identity");
        }

        fn release(&self) {
            std::fs::write(&self.release, b"release").unwrap();
        }
    }

    impl Drop for HeldChildren {
        fn drop(&mut self) {
            let mut pids = self.watched.clone();
            if let Ok(text) = std::fs::read_to_string(&self.pids) {
                pids.extend(
                    text.lines()
                        .filter_map(|line| line.trim().parse::<u32>().ok()),
                );
            }
            pids.sort_unstable();
            pids.dedup();
            for pid in pids {
                let _ = std::process::Command::new("kill")
                    .args(["-KILL", &pid.to_string()])
                    .status();
            }
        }
    }

    fn stop_process(pid: u32) {
        let status = std::process::Command::new("kill")
            .args(["-STOP", &pid.to_string()])
            .status()
            .expect("stopping the fixture supervisor");
        assert!(status.success(), "could not stop process {pid}");
    }

    fn process_alive(pid: u32) -> bool {
        PathBuf::from(format!("/proc/{pid}")).exists()
    }

    impl TwoRepoDeliveryFixture {
        fn new() -> Self {
            Self::with_count(2)
        }

        fn with_count(count: usize) -> Self {
            let temp = tempfile::tempdir().unwrap();
            let data = temp.path().join("data");
            let context = ServiceContext::open(
                RuntimePaths::new(data.clone()).unwrap(),
                Arc::new(crate::services::EnvironmentCredentials),
            )
            .unwrap();
            let router = SessionStore::new(context.db.clone())
                .create(temp.path(), "router-model", "fixture")
                .unwrap();
            role::set(&context.db, &router, Role::Router).unwrap();
            let mut repos = vec![
                Self::repo(temp.path(), "repo one", "develop", "repo-1", 0),
                // Paths contain spaces and the two targets use different branches.
                Self::repo(temp.path(), "repo two", "integration", "repo-2", 1),
            ];
            repos.truncate(count);
            let spec = TaskSpec {
                title: "Two repository delivery".into(),
                workspace: temp.path().to_path_buf(),
                targets: repos
                    .iter()
                    .map(|repo| repo.root.to_string_lossy().into_owned())
                    .collect(),
                goal: "Deliver both repositories".into(),
                requirements: vec![],
                acceptance_criteria: vec![crate::super_agent::model::AcceptanceCriterion {
                    id: "AC-001".into(),
                    description: "Both targets advance".into(),
                }],
                constraints: vec![],
                out_of_scope: vec![],
                target_branch: Some("develop".into()),
                repository_target_branches: repos
                    .iter()
                    .map(|repo| {
                        (
                            repo.root.to_string_lossy().into_owned(),
                            repo.branch.clone(),
                        )
                    })
                    .collect(),
            };
            let store = TaskStore::new(context.db.clone(), data.clone());
            let task = store.create_task(&router, spec, "create").unwrap();
            if count == 1 {
                let repo = &mut repos[0];
                git(
                    &repo.worktree,
                    &["branch", "-m", &format!("agent/{}", task.task_id)],
                );
                let managed = repo.worktree.parent().unwrap().join(&task.task_id);
                git(
                    &repo.root,
                    &[
                        "worktree",
                        "move",
                        repo.worktree.to_str().unwrap(),
                        managed.to_str().unwrap(),
                    ],
                );
                repo.worktree = managed;
            }
            for (index, repo) in repos.iter().enumerate() {
                let common = PathBuf::from(git(
                    &repo.root,
                    &["rev-parse", "--path-format=absolute", "--git-common-dir"],
                ))
                .canonicalize()
                .unwrap();
                store
                    .save_repo(&crate::super_agent::RepoTarget {
                        task_id: task.task_id.clone(),
                        repository_id: repo.repository_id.clone(),
                        repo_root: repo.root.clone(),
                        git_common_dir: common,
                        target_path: repo.root.clone(),
                        target_branch: repo.branch.clone(),
                        task_branch: if count == 1 {
                            format!("agent/{}", task.task_id)
                        } else {
                            format!("agent/{index}")
                        },
                        worktree_path: repo.worktree.clone(),
                        working_path: repo.worktree.clone(),
                        base_commit: repo.base.clone(),
                        submitted_commit: Some(repo.reviewed.clone()),
                        reviewed_commit: Some(repo.reviewed.clone()),
                        integrated_commit: Some(repo.reviewed.clone()),
                        integration_status: crate::super_agent::IntegrationStatus::Prepared,
                        verification_status: crate::super_agent::VerificationStatus::Passed,
                        source_state: vec![],
                    })
                    .unwrap();
            }
            let policy = Orchestrator::new(
                context.clone(),
                Self::config(),
                Self::provider(),
                "bypass".into(),
                Some(executable()),
            )
            .policy();
            // The task only reaches the delivery phase after the host-approved
            // implementation review and integration preparation; both are applied
            // through the real state machine so the seeded state is valid.
            store
                .db()
                .with_conn(|conn| {
                    conn.execute(
                        "UPDATE sa_tasks SET phase='IMPLEMENTING', status='RUNNING' WHERE task_id=?1",
                        [&task.task_id],
                    )
                })
                .unwrap()
                .unwrap();
            Self::record_evidence(&store, &task, &repos, &policy);
            let workspace = if count == 1 {
                repos[0].worktree.clone()
            } else {
                data.join("super-agent").join(&task.task_id).join("workspace")
            };
            std::fs::create_dir_all(&workspace).unwrap();
            let sessions = SessionStore::new(context.db.clone());
            let coder = sessions
                .create(&workspace, "router-model", "fixture")
                .unwrap();
            let reviewer = sessions
                .create(&workspace, "router-model", "fixture")
                .unwrap();
            sessions
                .append(&coder, "user", "implement the task")
                .unwrap();
            sessions
                .append(&reviewer, "user", "review the task")
                .unwrap();
            // Both members are Router children; record that ownership so a
            // resume passes the same parent check a live task would.
            crate::native_engine::child_session::record_adopted_ownership(
                &context,
                &coder,
                &router,
                "completed",
                "fixture-coder-job",
            )
            .unwrap();
            crate::native_engine::child_session::record_adopted_ownership(
                &context,
                &reviewer,
                &router,
                "completed",
                "fixture-reviewer-job",
            )
            .unwrap();
            store
                .set_sessions(&task.task_id, Some(&coder), Some(&reviewer))
                .unwrap();
            assert_eq!(
                store.get_task(&task.task_id).unwrap().unwrap().phase,
                Phase::DeliveryPending,
                "the seeded review and integration did not reach the delivery phase"
            );
            // The same executable plays both roles, exactly like `dcl`: with the
            // worker variable set it is the real production supervisor entry; as
            // the launched child it records one invocation and exits without
            // touching the repositories.
            use std::os::unix::fs::PermissionsExt;
            let supervisor = temp.path().join("dcl-fixture.sh");
            let test_binary = std::env::current_exe().unwrap();
            std::fs::write(
            &supervisor,
            format!(
                "#!/bin/sh\nif [ -n \"$DEEPSEEK_CODE_BACKGROUND_JOB\" ]; then\n  exec '{}' --exact 'services::background_sessions::tests::run_worker_entry' --ignored --nocapture\nfi\necho run >> '{}'\nexit 0\n",
                test_binary.display(),
                temp.path().join("child-launches.log").display()
            ),
        )
        .unwrap();
            std::fs::set_permissions(&supervisor, std::fs::Permissions::from_mode(0o755)).unwrap();
            Self {
                _temp: temp,
                data,
                context,
                router,
                task_id: task.task_id,
                repos,
                supervisor,
            }
        }

        fn repo(
            project: &Path,
            name: &str,
            branch: &str,
            repository_id: &str,
            index: usize,
        ) -> DeliveryRepoFixture {
            let root = project.join(name);
            std::fs::create_dir_all(&root).unwrap();
            git(&root, &["init", "-q", "-b", "main"]);
            git(&root, &["config", "user.email", "test@example.invalid"]);
            git(&root, &["config", "user.name", "Test"]);
            git(&root, &["config", "commit.gpgsign", "false"]);
            std::fs::write(root.join("base.txt"), "base\n").unwrap();
            git(&root, &["add", "."]);
            git(&root, &["commit", "-q", "-m", "base"]);
            let base = git(&root, &["rev-parse", "HEAD"]);
            git(&root, &["branch", branch, &base]);
            let common = PathBuf::from(git(
                &root,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            ));
            let worktree = common
                .join("desktop-worktrees")
                .join(format!("task-{index}"));
            git(
                &root,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    &format!("agent/{index}"),
                    &worktree.to_string_lossy(),
                    "main",
                ],
            );
            std::fs::write(worktree.join("reviewed.txt"), "value=1\n").unwrap();
            git(&worktree, &["add", "."]);
            git(&worktree, &["commit", "-q", "-m", "reviewed"]);
            let reviewed = git(&worktree, &["rev-parse", "HEAD"]);
            DeliveryRepoFixture {
                repository_id: repository_id.into(),
                root: root.canonicalize().unwrap(),
                worktree: worktree.canonicalize().unwrap(),
                branch: branch.into(),
                base,
                reviewed,
            }
        }

        fn record_evidence(
            store: &TaskStore,
            task: &TaskRecord,
            repos: &[DeliveryRepoFixture],
            policy: &crate::super_agent::state::StatePolicy,
        ) {
            let reviewed_versions: Vec<Value> = repos
                .iter()
                .map(|repo| {
                    json!({
                        "repository_id": repo.repository_id,
                        "commit": repo.reviewed,
                    })
                })
                .collect();
            let implementation = store
                .add_report(
                    &format!("{}-implementation", task.task_id),
                    &task.task_id,
                    crate::super_agent::model::ReportKind::Implementation,
                    task.spec_version,
                    task.review_round,
                    json!({
                        "spec_version": task.spec_version,
                        "round": task.review_round,
                        "summary": "fixture implementation",
                        "commits": ["HEAD"],
                    }),
                )
                .unwrap();
            assert!(!implementation.report_id.is_empty());
            store
                .apply_event(
                    &task.task_id,
                    NewEvent {
                        session_id: None,
                        run_id: None,
                        event_type: EventType::ImplementationSubmitted,
                        causation_id: None,
                        operation_id: format!("{}-implementation-event", task.task_id),
                        payload: json!({}),
                    },
                    policy,
                )
                .unwrap();
            let review = store
                .add_report(
                    &format!("{}-review", task.task_id),
                    &task.task_id,
                    crate::super_agent::model::ReportKind::Review,
                    task.spec_version,
                    task.review_round,
                    json!({
                        "verdict": "APPROVED",
                        "spec_version": task.spec_version,
                        "round": task.review_round,
                        "stage": "IMPLEMENTATION",
                        "reviewed_versions": reviewed_versions,
                        "acceptance_results": [],
                    }),
                )
                .unwrap();
            let harness = store
                .add_report(
                    &format!("{}-harness", task.task_id),
                    &task.task_id,
                    crate::super_agent::model::ReportKind::Harness,
                    task.spec_version,
                    task.review_round,
                    json!({
                        "spec_version": task.spec_version,
                        "round": task.review_round,
                        "stage": "IMPLEMENTATION",
                        "findings": [],
                    }),
                )
                .unwrap();
            let entries: Vec<Value> = repos
                .iter()
                .map(|repo| {
                    let tree = git(
                        &repo.root,
                        &["rev-parse", &format!("{}^{{tree}}", repo.reviewed)],
                    );
                    json!({
                        "repository_id": repo.repository_id,
                        "target_branch": repo.branch,
                        "target_ref": format!("refs/heads/{}", repo.branch),
                        "baseline_commit": repo.base,
                        "reviewed_commit": repo.reviewed,
                        "integrated_commit": repo.reviewed,
                        "integrated_tree": tree,
                        "tree_changed": false,
                        "content_review_reused": true,
                        "reason": "fixture preparation",
                    })
                })
                .collect();
            store
                .apply_event(
                    &task.task_id,
                    NewEvent {
                        session_id: None,
                        run_id: None,
                        event_type: EventType::ReviewCompleted,
                        causation_id: None,
                        operation_id: format!("{}-decision", task.task_id),
                        payload: json!({
                            "verdict": "APPROVED",
                            "stage": "IMPLEMENTATION",
                            "review_report_id": review.report_id,
                            "harness_report_id": harness.report_id,
                            "harness_blocked": false,
                        }),
                    },
                    policy,
                )
                .unwrap();
            store
                .apply_event(
                    &task.task_id,
                    NewEvent {
                        session_id: None,
                        run_id: None,
                        event_type: EventType::IntegrationPrepared,
                        causation_id: None,
                        operation_id: format!("{}-prepared", task.task_id),
                        payload: json!({"repositories": entries, "tree_changed": false}),
                    },
                    policy,
                )
                .unwrap();
        }

        fn store(&self) -> TaskStore {
            TaskStore::new(self.context.db.clone(), self.data.clone())
        }

        fn task(&self) -> TaskRecord {
            self.store().get_task(&self.task_id).unwrap().unwrap()
        }

        fn prepare_plan(&self) -> DeliveryPlan {
            match crate::super_agent::delivery::prepare_delivery(&self.store(), &self.task())
                .unwrap()
            {
                crate::super_agent::delivery::DeliveryPreflight::Planned(plan) => plan,
                other => panic!("expected a prepared delivery plan, got {other:?}"),
            }
        }

        fn set_target(&self, index: usize, oid: &str) {
            let repo = &self.repos[index];
            git(
                &repo.root,
                &["update-ref", &format!("refs/heads/{}", repo.branch), oid],
            );
        }

        /// Moves one target to an unrelated commit from an isolated worktree, as an
        /// external process would after the host observed it.
        fn move_target_elsewhere(&self, index: usize) -> String {
            let repo = &self.repos[index];
            let path = self._temp.path().join(format!("move-{index}"));
            git(
                &repo.root,
                &[
                    "worktree",
                    "add",
                    "-q",
                    &path.to_string_lossy(),
                    &repo.branch,
                ],
            );
            git(&path, &["commit", "-q", "--allow-empty", "-m", "elsewhere"]);
            let head = git(&path, &["rev-parse", "HEAD"]);
            git(
                &repo.root,
                &["worktree", "remove", "--force", &path.to_string_lossy()],
            );
            head
        }

        /// Checks out one target branch in a linked worktree. It must run before
        /// [`Self::prepare_plan`] so the recorded plan expects a checkout.
        fn checkout_target(&self, index: usize) -> PathBuf {
            let repo = &self.repos[index];
            let path = self._temp.path().join(format!("target checkout {index}"));
            git(
                &repo.root,
                &[
                    "worktree",
                    "add",
                    "-q",
                    &path.to_string_lossy(),
                    &repo.branch,
                ],
            );
            path.canonicalize().unwrap()
        }

        fn target_plan<'a>(&self, plan: &'a DeliveryPlan, index: usize) -> &'a DeliveryTargetPlan {
            plan.repositories
                .iter()
                .find(|target| target.repository_id == self.repos[index].repository_id)
                .unwrap()
        }

        /// Records the host-verified delivery of every planned repository and
        /// applies it through the real state machine. A repository that must
        /// move from its approved baseline is dispatched through the real
        /// launcher, so its durable Coder receipt is the production one; a
        /// target already at its approved commit needs no dispatch and gets no
        /// receipt.
        async fn deliver_all(&self, plan: &DeliveryPlan) {
            let task = self.task();
            let coder = task.coder.session_id.clone().expect("coder session");
            let launcher = self.orchestrator().launcher();
            let mut dispatched = false;
            for target in &plan.repositories {
                if target.old_oid == target.new_oid {
                    continue;
                }
                let identity = DeliveryOperationIdentity::for_target(plan, target, &coder);
                self.store()
                    .begin_repository_dispatch(&DeliveryDispatch {
                        task_id: self.task_id.clone(),
                        spec_version: plan.spec_version,
                        review_round: plan.review_round,
                        plan_operation_id: plan.operation_id.clone(),
                        coder_session_id: coder.clone(),
                        purpose: identity.purpose_key(),
                        repository_id: Some(target.repository_id.clone()),
                        old_oid: Some(target.old_oid.clone()),
                        new_oid: Some(target.new_oid.clone()),
                        attempt: target.attempt,
                        job_id: None,
                        created_at: "2026-01-01T00:00:00Z".into(),
                    })
                    .unwrap();
                let outcome = launcher
                    .resume_coder_for_delivery(&task, "fixture delivery", &identity)
                    .await
                    .unwrap();
                self.store()
                    .set_repository_dispatch_job(
                        &self.task_id,
                        &plan.operation_id,
                        &target.repository_id,
                        &outcome.job_id,
                    )
                    .unwrap();
                dispatched = true;
            }
            if dispatched {
                self.wait_settled().await;
            }
            for (index, repo) in self.repos.iter().enumerate() {
                self.set_target(index, &repo.reviewed);
                self.record_observation(plan, index);
            }
            self.store()
                .record_delivery_completed(
                    plan,
                    &format!("{}-delivered", self.task_id),
                    json!({"complete": true}),
                )
                .unwrap();
            self.store()
                .process_pending(
                    &self.router,
                    &crate::super_agent::state::StatePolicy::default(),
                )
                .unwrap();
            assert_eq!(self.task().phase, Phase::Finalizing);
            assert_eq!(self.task().status, Status::Running);
        }

        /// The final-delivery review and harness the same Reviewer submits.
        fn final_reports(&self, plan: &DeliveryPlan, approved: bool) {
            self.final_review_report(plan, approved);
            self.final_harness_report();
        }

        fn final_review_report(&self, plan: &DeliveryPlan, approved: bool) {
            let task = self.task();
            let versions: Vec<Value> = plan
                .repositories
                .iter()
                .map(|target| {
                    json!({
                        "repository_id": target.repository_id,
                        "commit": target.new_oid,
                    })
                })
                .collect();
            let verdict = if approved {
                "APPROVED"
            } else {
                "CHANGES_REQUESTED"
            };
            self.store()
                .add_report(
                    &format!("{}-final-review", self.task_id),
                    &self.task_id,
                    crate::super_agent::model::ReportKind::Review,
                    task.spec_version,
                    task.review_round,
                    json!({
                        "verdict": verdict,
                        "spec_version": task.spec_version,
                        "round": task.review_round,
                        "stage": "FINAL_DELIVERY",
                        "reviewed_versions": versions,
                        "acceptance_results": [{
                            "criterion": "AC-001",
                            "passed": approved,
                            "evidence": ["git rev-parse develop"],
                        }],
                        "issues": [],
                        "verification": ["git rev-parse develop"],
                        "limitations": [],
                    }),
                )
                .unwrap();
        }

        fn final_harness_report(&self) {
            let task = self.task();
            self.store()
                .add_report(
                    &format!("{}-final-harness", self.task_id),
                    &self.task_id,
                    crate::super_agent::model::ReportKind::Harness,
                    task.spec_version,
                    task.review_round,
                    json!({
                        "spec_version": task.spec_version,
                        "round": task.review_round,
                        "stage": "FINAL_DELIVERY",
                        "findings": [],
                    }),
                )
                .unwrap();
        }

        /// Makes every future child exit with this code, as a Reviewer that
        /// dies would.
        fn fail_future_children(&self, code: i32) {
            use std::os::unix::fs::PermissionsExt;
            let test_binary = std::env::current_exe().unwrap();
            std::fs::write(
                &self.supervisor,
                format!(
                    "#!/bin/sh\nif [ -n \"$DEEPSEEK_CODE_BACKGROUND_JOB\" ]; then\n  exec '{}' --exact 'services::background_sessions::tests::run_worker_entry' --ignored --nocapture\nfi\nexit {code}\n",
                    test_binary.display()
                ),
            )
            .unwrap();
            std::fs::set_permissions(&self.supervisor, std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }

        fn final_completion_marker(&self) -> Value {
            self.context
                .setting::<Value>(&format!(
                    "super-agent.reviewer-resume.{}.{}.{}.final-completion",
                    self.task_id,
                    self.task().spec_version,
                    self.task().review_round
                ))
                .unwrap()
                .unwrap_or(Value::Null)
        }

        /// Replaces the supervisor with one that keeps the launched child alive
        /// until the guard releases it, and records the child and supervisor
        /// PIDs in a fixture-owned file.
        fn hold_future_children(&self) -> HeldChildren {
            use std::os::unix::fs::PermissionsExt;
            let pids = self._temp.path().join("held-child-pids");
            let release = self._temp.path().join("release-held-child");
            let test_binary = std::env::current_exe().unwrap();
            std::fs::write(
                &self.supervisor,
                format!(
                    "#!/bin/sh\nif [ -n \"$DEEPSEEK_CODE_BACKGROUND_JOB\" ]; then\n  exec '{}' --exact 'services::background_sessions::tests::run_worker_entry' --ignored --nocapture\nfi\nprintf '%s\\n%s\\n' \"$$\" \"$PPID\" > '{}'\nwhile [ ! -f '{}' ]; do sleep 0.05; done\nexit 0\n",
                    test_binary.display(),
                    pids.display(),
                    release.display()
                ),
            )
            .unwrap();
            std::fs::set_permissions(&self.supervisor, std::fs::Permissions::from_mode(0o755))
                .unwrap();
            HeldChildren {
                pids,
                release,
                watched: Vec::new(),
            }
        }

        /// The durable directory of one reserved job. The test damages only
        /// `state.json`, leaving launch and identity files in place.
        fn job_directory(&self, job_id: &str) -> PathBuf {
            crate::services::background_sessions::directory(&self.data, job_id).unwrap()
        }

        fn task_completed_count(&self) -> usize {
            self.store()
                .events(&self.task_id)
                .unwrap()
                .iter()
                .filter(|event| event.event_type == EventType::TaskCompleted)
                .count()
        }

        fn record_observation(&self, plan: &DeliveryPlan, index: usize) {
            let target = plan
                .repositories
                .iter()
                .find(|target| target.repository_id == self.repos[index].repository_id)
                .unwrap();
            self.store()
                .record_repository_delivery_observed(
                    &crate::super_agent::delivery::RepositoryDeliveryObservation {
                        task_id: plan.task_id.clone(),
                        spec_version: plan.spec_version,
                        review_round: plan.review_round,
                        plan_operation_id: plan.operation_id.clone(),
                        repository_id: target.repository_id.clone(),
                        old_oid: target.old_oid.clone(),
                        new_oid: target.new_oid.clone(),
                        target_ref: target.target_ref.clone(),
                        coder_session_id: self.task().coder.session_id.unwrap(),
                        attempt: target.attempt,
                        verified: true,
                        // The fixture dispatches a real job for a repository
                        // that must move, and never for one already at the
                        // approved commit.
                        source: if target.old_oid == target.new_oid {
                            crate::super_agent::delivery::DeliveryObservationSource::HostVerifiedAlreadyAtTarget
                        } else {
                            crate::super_agent::delivery::DeliveryObservationSource::DeliveryJob
                        },
                        observed_at: "2026-01-01T00:00:00Z".into(),
                    },
                )
                .unwrap();
        }

        fn config() -> SuperAgentConfig {
            let mut config: SuperAgentConfig =
                crate::super_agent::config::parse(&json!({"agents":{}})).unwrap();
            config.delivery.enabled = true;
            config
        }

        fn provider() -> ResolvedProvider {
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

        fn orchestrator(&self) -> Orchestrator {
            Orchestrator::new(
                self.context.clone(),
                Self::config(),
                Self::provider(),
                "bypass".into(),
                Some(self.supervisor.clone()),
            )
        }

        fn completed(&self) -> bool {
            self.store()
                .events(&self.task_id)
                .unwrap()
                .iter()
                .any(|event| event.event_type == EventType::IntegrationCompleted)
        }

        fn observation(&self, plan: &DeliveryPlan, index: usize) -> bool {
            self.store()
                .repository_delivery_observation(
                    &self.task_id,
                    &plan.operation_id,
                    &self.repos[index].repository_id,
                )
                .unwrap()
                .is_some()
        }

        async fn wait_settled(&self) {
            for _ in 0..800 {
                let jobs =
                    crate::services::background_sessions::list(&self.data, Some(&self.router))
                        .unwrap_or_default();
                if !jobs.is_empty()
                    && jobs.iter().all(|job| {
                        !matches!(job.status.as_str(), "starting" | "running" | "unresponsive")
                    })
                {
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
            panic!("the delivery job did not settle");
        }
    }

    #[tokio::test]
    async fn delivery_advances_two_repositories_one_at_a_time() {
        let fixture = TwoRepoDeliveryFixture::new();
        let plan = fixture.prepare_plan();
        // A is delivered externally; B stays at its approved baseline.
        fixture.set_target(0, &fixture.repos[0].reviewed);
        let orchestrator = fixture.orchestrator();

        let first = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(first.blocked, Vec::<String>::new(), "{first:?}");
        let task = fixture.task();
        assert_eq!(
            task.phase,
            Phase::DeliveryPending,
            "one delivered repository must not finish the task"
        );
        assert!(
            fixture.observation(&plan, 0),
            "the delivered repository has no durable observation"
        );
        assert!(
            !fixture.observation(&plan, 1),
            "the pending repository was observed without a delivery"
        );
        assert_eq!(
            task.repositories[0].integration_status,
            crate::super_agent::IntegrationStatus::Completed
        );
        assert_eq!(
            task.repositories[1].integration_status,
            crate::super_agent::IntegrationStatus::Prepared
        );
        assert!(
            !fixture.completed(),
            "a partial delivery emitted the global event"
        );
        assert!(
            crate::services::background_sessions::list(&fixture.data, Some(&fixture.router))
                .unwrap_or_default()
                .is_empty(),
            "an already delivered repository started a child"
        );

        // B reaches its approved commit as its own child would have left it.
        fixture.set_target(1, &fixture.repos[1].reviewed);
        let second = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(second.blocked, Vec::<String>::new(), "{second:?}");
        let task = fixture.task();
        assert_eq!(task.phase, Phase::Finalizing);
        assert_eq!(task.status, Status::Running);
        for repo in &task.repositories {
            assert_eq!(
                repo.integration_status,
                crate::super_agent::IntegrationStatus::Completed
            );
        }
        assert!(
            fixture.completed(),
            "the complete delivery was not recorded"
        );
        assert!(!fixture
            .store()
            .events(&fixture.task_id)
            .unwrap()
            .iter()
            .any(|event| event.event_type == EventType::TaskCompleted));
        assert_eq!(
            git(
                &fixture.repos[0].root,
                &[
                    "rev-parse",
                    &format!("refs/heads/{}", fixture.repos[0].branch)
                ]
            ),
            fixture.repos[0].reviewed
        );
        assert_eq!(
            git(
                &fixture.repos[1].root,
                &[
                    "rev-parse",
                    &format!("refs/heads/{}", fixture.repos[1].branch)
                ]
            ),
            fixture.repos[1].reviewed
        );
    }

    #[tokio::test]
    async fn a_failed_second_repository_preserves_the_first_delivery() {
        let fixture = TwoRepoDeliveryFixture::new();
        let plan = fixture.prepare_plan();
        fixture.set_target(0, &fixture.repos[0].reviewed);
        fixture.record_observation(&plan, 0);
        let orchestrator = fixture.orchestrator();

        // B's real child starts and finishes without moving its target.
        let first = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(first.blocked, Vec::<String>::new(), "{first:?}");
        fixture.wait_settled().await;
        let second = orchestrator.tick(&fixture.router).await.unwrap();
        assert!(
            second.blocked.iter().any(|reason| {
                reason.contains(&fixture.repos[1].repository_id)
                    && reason.contains("approved integrated commit")
            }),
            "the failing repository was not named: {second:?}"
        );
        let task = fixture.task();
        assert_eq!(task.status, Status::Blocked);
        assert_eq!(task.phase, Phase::DeliveryPending);
        assert_eq!(
            task.repositories[0].integration_status,
            crate::super_agent::IntegrationStatus::Completed,
            "A's completed progress was lost"
        );
        assert_eq!(
            task.repositories[1].integration_status,
            crate::super_agent::IntegrationStatus::Prepared
        );
        assert!(fixture.observation(&plan, 0), "A's observation was removed");
        assert!(
            !fixture.observation(&plan, 1),
            "the failed repository was observed as delivered"
        );
        assert!(!fixture.completed(), "a failed delivery emitted completion");
        // A's real commit is preserved; B's target is untouched.
        assert_eq!(
            git(
                &fixture.repos[0].root,
                &[
                    "rev-parse",
                    &format!("refs/heads/{}", fixture.repos[0].branch)
                ]
            ),
            fixture.repos[0].reviewed
        );
        assert_eq!(
            git(
                &fixture.repos[1].root,
                &[
                    "rev-parse",
                    &format!("refs/heads/{}", fixture.repos[1].branch)
                ]
            ),
            fixture.repos[1].base
        );
        // Repeated ticks do not invent a retry.
        let jobs = crate::services::background_sessions::list(&fixture.data, Some(&fixture.router))
            .unwrap_or_default()
            .len();
        let _ = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(
            crate::services::background_sessions::list(&fixture.data, Some(&fixture.router))
                .unwrap_or_default()
                .len(),
            jobs,
            "a blocked failed repository started another child"
        );
    }

    #[tokio::test]
    async fn a_repository_that_drifts_after_observation_blocks_final_completion() {
        let fixture = TwoRepoDeliveryFixture::new();
        let plan = fixture.prepare_plan();
        fixture.set_target(0, &fixture.repos[0].reviewed);
        fixture.record_observation(&plan, 0);
        fixture.set_target(1, &fixture.repos[1].reviewed);
        // A moved again after the host observed it; the approved evidence is not
        // refreshed and no new observation replaces the old one.
        let elsewhere = fixture.move_target_elsewhere(0);
        let orchestrator = fixture.orchestrator();

        let report = orchestrator.tick(&fixture.router).await.unwrap();
        assert!(
            report.blocked.iter().any(|reason| {
                reason.contains(&fixture.repos[0].repository_id)
                    && reason.contains("fresh verification")
                    && reason.contains("approved integrated commit")
            }),
            "the drifted repository was not named: {report:?}"
        );
        let task = fixture.task();
        assert_eq!(task.status, Status::Blocked);
        assert_eq!(task.phase, Phase::DeliveryPending);
        assert!(!fixture.completed(), "stale evidence completed the task");
        assert!(fixture.observation(&plan, 0));
        assert!(
            fixture.observation(&plan, 1),
            "B's genuine delivery was not observed"
        );
        assert_eq!(
            git(
                &fixture.repos[0].root,
                &[
                    "rev-parse",
                    &format!("refs/heads/{}", fixture.repos[0].branch)
                ]
            ),
            elsewhere,
            "the failed final verification rewrote the real target"
        );
    }

    #[tokio::test]
    async fn a_task_missing_a_planned_repository_cannot_deliver() {
        let fixture = TwoRepoDeliveryFixture::new();
        let plan = fixture.prepare_plan();
        fixture
            .store()
            .db()
            .with_conn(|conn| {
                conn.execute(
                    "DELETE FROM sa_task_repos WHERE task_id=?1 AND repository_id=?2",
                    [&fixture.task_id, &fixture.repos[1].repository_id],
                )
            })
            .unwrap()
            .unwrap();
        let orchestrator = fixture.orchestrator();

        let report = orchestrator.tick(&fixture.router).await.unwrap();
        assert!(
            report.blocked.iter().any(|reason| {
                // The binding now rejects a narrowed repository set itself,
                // and must still name the repository the task no longer lists.
                (reason.contains("no longer lists repository") || reason.contains("repository set"))
                    && reason.contains(&fixture.repos[1].repository_id)
            }),
            "a narrowed task must not deliver the full plan: {report:?}"
        );
        assert!(
            !fixture.observation(&plan, 0) && !fixture.observation(&plan, 1),
            "a partially listed task produced observations"
        );
        assert!(!fixture.completed());
        assert!(
            crate::services::background_sessions::list(&fixture.data, Some(&fixture.router))
                .unwrap_or_default()
                .is_empty(),
            "a partially listed task started a child"
        );
    }

    /// Asserts a blocked pending B: named repository, no child, A's observation
    /// and delivered ref intact, B still prepared, and no global event.
    fn assert_blocked_b_without_dispatch(
        fixture: &TwoRepoDeliveryFixture,
        plan: &DeliveryPlan,
        report: &crate::super_agent::orchestrator::TickReport,
        expected: &[&str],
        label: &str,
    ) {
        for needle in expected {
            assert!(
                report.blocked.iter().any(|reason| {
                    reason.contains(&fixture.repos[1].repository_id) && reason.contains(needle)
                }),
                "{label}: the block did not name B with {needle:?}: {report:?}"
            );
        }
        assert!(
            crate::services::background_sessions::list(&fixture.data, Some(&fixture.router))
                .unwrap_or_default()
                .is_empty(),
            "{label}: a child was started for an unready B"
        );
        assert!(
            fixture.observation(plan, 0),
            "{label}: A's durable observation was removed"
        );
        assert!(
            !fixture.observation(plan, 1),
            "{label}: the blocked B was observed as delivered"
        );
        assert_eq!(
            git(
                &fixture.repos[0].root,
                &[
                    "rev-parse",
                    &format!("refs/heads/{}", fixture.repos[0].branch)
                ]
            ),
            fixture.repos[0].reviewed,
            "{label}: A's delivered ref changed"
        );
        let task = fixture.task();
        assert_eq!(
            task.status,
            Status::Blocked,
            "{label}: the task was not blocked"
        );
        assert_eq!(task.phase, Phase::DeliveryPending);
        assert_eq!(
            task.repositories[0].integration_status,
            crate::super_agent::IntegrationStatus::Completed,
            "{label}: A's completed progress was lost"
        );
        assert_eq!(
            task.repositories[1].integration_status,
            crate::super_agent::IntegrationStatus::Prepared
        );
        assert!(
            !fixture.completed(),
            "{label}: a global completion event was written"
        );
    }

    #[tokio::test]
    async fn a_pending_repository_with_a_dirty_checkout_blocks_before_dispatch() {
        let fixture = TwoRepoDeliveryFixture::new();
        // The plan records B as checked out, then the checkout becomes dirty.
        let checkout = fixture.checkout_target(1);
        let plan = fixture.prepare_plan();
        let target_b = fixture.target_plan(&plan, 1).clone();
        assert!(
            matches!(target_b.destination, DeliveryDestination::CheckedOut { .. }),
            "the fixture did not record B as checked out"
        );
        fixture.set_target(0, &fixture.repos[0].reviewed);
        fixture.record_observation(&plan, 0);
        std::fs::write(checkout.join("base.txt"), "dirty\n").unwrap();
        let plans_before = fixture.store().delivery_plans(&fixture.task_id).unwrap();
        let orchestrator = fixture.orchestrator();

        let report = orchestrator.tick(&fixture.router).await.unwrap();
        assert_blocked_b_without_dispatch(
            &fixture,
            &plan,
            &report,
            &["uncommitted or untracked"],
            "dirty B checkout",
        );
        assert_eq!(
            fixture.store().delivery_plans(&fixture.task_id).unwrap(),
            plans_before,
            "the blocked attempt rewrote the immutable plan"
        );
        assert_eq!(
            fixture.store().lock_holder(&target_b.lock_key).unwrap(),
            Some((fixture.task_id.clone(), target_b.lock_operation_id.clone())),
            "the blocked attempt changed B's cooperative ownership"
        );
        assert_eq!(
            git(
                &fixture.repos[1].root,
                &[
                    "rev-parse",
                    &format!("refs/heads/{}", fixture.repos[1].branch)
                ]
            ),
            fixture.repos[1].base,
            "the blocked attempt moved B's target"
        );
        assert_eq!(
            std::fs::read_to_string(checkout.join("base.txt")).unwrap(),
            "dirty\n",
            "the blocked attempt touched the dirty checkout"
        );
    }

    #[tokio::test]
    async fn a_pending_repository_whose_ref_moved_blocks_before_dispatch() {
        let fixture = TwoRepoDeliveryFixture::new();
        let plan = fixture.prepare_plan();
        let target_b = fixture.target_plan(&plan, 1).clone();
        fixture.set_target(0, &fixture.repos[0].reviewed);
        fixture.record_observation(&plan, 0);
        // An external process moves B after A's observation.
        let elsewhere = fixture.move_target_elsewhere(1);
        assert_ne!(elsewhere, fixture.repos[1].base);
        let plans_before = fixture.store().delivery_plans(&fixture.task_id).unwrap();
        let orchestrator = fixture.orchestrator();

        let report = orchestrator.tick(&fixture.router).await.unwrap();
        assert_blocked_b_without_dispatch(
            &fixture,
            &plan,
            &report,
            &["moved", "approved plan is stale"],
            "moved B ref",
        );
        assert_eq!(
            fixture.store().delivery_plans(&fixture.task_id).unwrap(),
            plans_before,
            "the blocked attempt rewrote the immutable plan"
        );
        let stored = fixture
            .store()
            .delivery_plan(&fixture.task_id, &plan.operation_id)
            .unwrap()
            .expect("persisted plan");
        let stored_b = fixture.target_plan(&stored, 1);
        assert_eq!(
            stored_b.old_oid, fixture.repos[1].base,
            "the blocked attempt refreshed B's approved baseline"
        );
        assert_eq!(
            stored_b.new_oid, fixture.repos[1].reviewed,
            "the blocked attempt replaced B's approved commit"
        );
        assert_eq!(
            git(
                &fixture.repos[1].root,
                &[
                    "rev-parse",
                    &format!("refs/heads/{}", fixture.repos[1].branch)
                ]
            ),
            elsewhere,
            "the blocked attempt moved B's target"
        );
        assert_eq!(
            fixture.store().lock_holder(&target_b.lock_key).unwrap(),
            Some((fixture.task_id.clone(), target_b.lock_operation_id.clone())),
            "the blocked attempt changed B's cooperative ownership"
        );
    }

    #[tokio::test]
    async fn a_pending_repository_without_cooperative_ownership_blocks_before_dispatch() {
        // B's own lock was released after A was observed.
        {
            let fixture = TwoRepoDeliveryFixture::new();
            let plan = fixture.prepare_plan();
            let target_b = fixture.target_plan(&plan, 1).clone();
            fixture.set_target(0, &fixture.repos[0].reviewed);
            fixture.record_observation(&plan, 0);
            assert!(fixture
                .store()
                .unlock(
                    &target_b.lock_key,
                    &fixture.task_id,
                    &target_b.lock_operation_id
                )
                .unwrap());
            let plans_before = fixture.store().delivery_plans(&fixture.task_id).unwrap();
            let orchestrator = fixture.orchestrator();

            let report = orchestrator.tick(&fixture.router).await.unwrap();
            assert_blocked_b_without_dispatch(
                &fixture,
                &plan,
                &report,
                &["is not owned by this delivery"],
                "released B lock",
            );
            assert_eq!(
                fixture.store().lock_holder(&target_b.lock_key).unwrap(),
                None,
                "the blocked attempt reacquired B's released lock"
            );
            assert_eq!(
                fixture.store().delivery_plans(&fixture.task_id).unwrap(),
                plans_before,
                "the blocked attempt rewrote the immutable plan"
            );
            assert_eq!(
                git(
                    &fixture.repos[1].root,
                    &[
                        "rev-parse",
                        &format!("refs/heads/{}", fixture.repos[1].branch)
                    ]
                ),
                fixture.repos[1].base,
                "the blocked attempt moved B's target"
            );
        }

        // Another task holds B's lock; it is named and never stolen.
        {
            let fixture = TwoRepoDeliveryFixture::new();
            let plan = fixture.prepare_plan();
            let target_b = fixture.target_plan(&plan, 1).clone();
            fixture.set_target(0, &fixture.repos[0].reviewed);
            fixture.record_observation(&plan, 0);
            assert!(fixture
                .store()
                .unlock(
                    &target_b.lock_key,
                    &fixture.task_id,
                    &target_b.lock_operation_id
                )
                .unwrap());
            fixture
                .store()
                .try_lock("other:operation", &target_b.lock_key, "other-task")
                .unwrap();
            let plans_before = fixture.store().delivery_plans(&fixture.task_id).unwrap();
            let orchestrator = fixture.orchestrator();

            let report = orchestrator.tick(&fixture.router).await.unwrap();
            assert_blocked_b_without_dispatch(
                &fixture,
                &plan,
                &report,
                &["held by task other-task"],
                "stolen B lock",
            );
            assert_eq!(
                fixture.store().lock_holder(&target_b.lock_key).unwrap(),
                Some(("other-task".to_owned(), "other:operation".to_owned())),
                "the blocked attempt stole the other task's lock"
            );
            assert_eq!(
                fixture.store().delivery_plans(&fixture.task_id).unwrap(),
                plans_before,
                "the blocked attempt rewrote the immutable plan"
            );
            assert_eq!(
                git(
                    &fixture.repos[1].root,
                    &[
                        "rev-parse",
                        &format!("refs/heads/{}", fixture.repos[1].branch)
                    ]
                ),
                fixture.repos[1].base,
                "the blocked attempt moved B's target"
            );
        }
    }

    #[tokio::test]
    async fn a_content_identical_delivery_completes_only_through_a_real_final_review() {
        let fixture = TwoRepoDeliveryFixture::new();
        let reviewer = fixture
            .task()
            .reviewer
            .session_id
            .clone()
            .expect("reviewer session");
        let plan = fixture.prepare_plan();
        fixture.deliver_all(&plan).await;
        let orchestrator = fixture.orchestrator();

        // No final-delivery review exists yet: the same Reviewer is resumed
        // once, and nothing is completed from the implementation approval.
        let first = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(first.blocked, Vec::<String>::new(), "{first:?}");
        let task = fixture.task();
        assert_eq!(task.phase, Phase::Finalizing);
        assert_eq!(task.status, Status::Running);
        assert_eq!(fixture.task_completed_count(), 0);
        let jobs = crate::services::background_sessions::list(&fixture.data, Some(&fixture.router))
            .unwrap_or_default();
        let reviewer_jobs: Vec<_> = jobs
            .iter()
            .filter(|job| job.session_id == reviewer)
            .collect();
        assert_eq!(
            reviewer_jobs.len(),
            1,
            "the completion review must resume the existing Reviewer once"
        );
        fixture.wait_settled().await;

        // The Reviewer submits its own review and harness of the delivered
        // OIDs. The decision is a durable event applied by the next tick.
        fixture.final_reports(&plan, true);
        let deciding = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(deciding.blocked, Vec::<String>::new(), "{deciding:?}");
        let report = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(report.blocked, Vec::<String>::new(), "{report:?}");

        let task = fixture.task();
        assert_eq!(task.phase, Phase::Finalizing);
        assert_eq!(task.status, Status::Completed);
        assert_eq!(fixture.task_completed_count(), 1);
        assert!(!fixture
            .store()
            .events(&fixture.task_id)
            .unwrap()
            .iter()
            .any(|event| event.event_type == EventType::CleanupCompleted));
        // The delivered OIDs are the reviewed ones; nothing was rewritten.
        for (index, repo) in task.repositories.iter().enumerate() {
            assert_eq!(
                repo.integration_status,
                crate::super_agent::IntegrationStatus::Completed
            );
            assert_eq!(
                repo.reviewed_commit.as_deref(),
                Some(fixture.repos[index].reviewed.as_str())
            );
            assert_eq!(
                repo.integrated_commit.as_deref(),
                Some(fixture.repos[index].reviewed.as_str())
            );
        }

        // Extra ticks and a reopen never duplicate completion or replay
        // delivery.
        let delivery_events = fixture
            .store()
            .events(&fixture.task_id)
            .unwrap()
            .iter()
            .filter(|event| event.event_type == EventType::IntegrationCompleted)
            .count();
        let _ = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(fixture.task_completed_count(), 1);
        let reopened_context = crate::services::ServiceContext::open(
            RuntimePaths::new(fixture.data.clone()).unwrap(),
            Arc::new(crate::services::EnvironmentCredentials),
        )
        .unwrap();
        let reopened_store = TaskStore::new(reopened_context.db.clone(), fixture.data.clone());
        let reopened = Orchestrator::new(
            reopened_context.clone(),
            TwoRepoDeliveryFixture::config(),
            TwoRepoDeliveryFixture::provider(),
            "bypass".into(),
            Some(fixture.supervisor.clone()),
        );
        let reopened_report = reopened.tick(&fixture.router).await.unwrap();
        assert_eq!(
            reopened_report.blocked,
            Vec::<String>::new(),
            "{reopened_report:?}"
        );
        assert_eq!(
            reopened_store
                .get_task(&fixture.task_id)
                .unwrap()
                .unwrap()
                .status,
            Status::Completed
        );
        assert_eq!(
            reopened_store
                .events(&fixture.task_id)
                .unwrap()
                .iter()
                .filter(|event| event.event_type == EventType::TaskCompleted)
                .count(),
            1
        );
        assert_eq!(
            reopened_store
                .events(&fixture.task_id)
                .unwrap()
                .iter()
                .filter(|event| event.event_type == EventType::IntegrationCompleted)
                .count(),
            delivery_events
        );
    }

    /// Both final reports can arrive from a Reviewer whose process is still
    /// alive. The completion decision must inspect that exact known job
    /// directly: removing only its own `state.json` may not make the discovery
    /// list's silence count as proof that the child stopped.
    #[tokio::test]
    async fn a_live_completion_review_with_destroyed_state_never_completes() {
        let fixture = TwoRepoDeliveryFixture::new();
        let plan = fixture.prepare_plan();
        fixture.deliver_all(&plan).await;
        let mut held = fixture.hold_future_children();
        let orchestrator = fixture.orchestrator();

        // The real launcher starts the post-delivery completion review and the
        // fixture keeps that genuine child running.
        let launched = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(launched.blocked, Vec::<String>::new(), "{launched:?}");
        let (child, supervisor) = held.wait_for_pids();
        let job_id = fixture.final_completion_marker()["job_id"]
            .as_str()
            .expect("completion review job")
            .to_owned();

        // Both valid FINAL_DELIVERY reports exist while the child is alive.
        fixture.final_reports(&plan, true);
        let deciding = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(deciding.blocked, Vec::<String>::new(), "{deciding:?}");
        assert_eq!(fixture.task_completed_count(), 0);

        // Freeze the supervisor so it cannot republish, then damage only this
        // known job's state file. The genuine child keeps running.
        stop_process(supervisor);
        let state = fixture.job_directory(&job_id).join("state.json");
        assert!(state.exists(), "the genuine job state must exist");
        std::fs::remove_file(&state).unwrap();
        assert!(!state.exists());
        assert!(process_alive(child), "the reviewer child must still be alive");

        let blocked = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(
            fixture.task_completed_count(),
            0,
            "a reviewer whose own durable state was removed was treated as stopped: {blocked:?}"
        );
        assert!(
            process_alive(child),
            "the completion check must not complete while the genuine child is alive"
        );
        assert!(
            blocked.blocked.iter().any(|reason| {
                reason.contains(&job_id) && reason.contains("no durable task state")
            }),
            "the damaged known job must block with its exact identity: {blocked:?}"
        );
        let task = fixture.task();
        assert_eq!(task.status, Status::Blocked);
        assert_eq!(task.phase, Phase::Finalizing);

        // A reopened host must keep the requirement instead of completing from
        // the reports it can still read.
        let reopened_context = crate::services::ServiceContext::open(
            RuntimePaths::new(fixture.data.clone()).unwrap(),
            Arc::new(crate::services::EnvironmentCredentials),
        )
        .unwrap();
        let reopened_store = TaskStore::new(reopened_context.db.clone(), fixture.data.clone());
        let reopened = Orchestrator::new(
            reopened_context,
            TwoRepoDeliveryFixture::config(),
            TwoRepoDeliveryFixture::provider(),
            "bypass".into(),
            Some(fixture.supervisor.clone()),
        );
        let _ = reopened.tick(&fixture.router).await.unwrap();
        assert_eq!(fixture.task_completed_count(), 0);
        assert_eq!(
            reopened_store
                .get_task(&fixture.task_id)
                .unwrap()
                .unwrap()
                .status,
            Status::Blocked
        );
        assert!(process_alive(child), "the held child was lost");
        held.release();
    }

    /// Older terminal receipts must not stand in for the current completion
    /// review. Removing both the current marker and its launch operation while
    /// the genuine held child's state is damaged leaves the receipt collection
    /// non-empty, so coverage is checked against the plan's required stage.
    #[tokio::test]
    async fn a_missing_current_completion_review_receipt_blocks_with_older_receipts_present() {
        let fixture = TwoRepoDeliveryFixture::new();
        let plan = fixture.prepare_plan();
        fixture.deliver_all(&plan).await;
        // A genuine, terminal Coder integration receipt is preserved as the
        // "older receipt" that must not satisfy the Reviewer requirement.
        let older = fixture
            .orchestrator()
            .launcher()
            .resume_coder(&fixture.task(), "fixture integration", "integration")
            .await
            .unwrap();
        assert!(older.fresh);
        fixture.wait_settled().await;

        let mut held = fixture.hold_future_children();
        let orchestrator = fixture.orchestrator();
        let launched = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(launched.blocked, Vec::<String>::new(), "{launched:?}");
        let (child, supervisor) = held.wait_for_pids();
        let job_id = fixture.final_completion_marker()["job_id"]
            .as_str()
            .expect("completion review job")
            .to_owned();
        fixture.final_reports(&plan, true);
        let deciding = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(deciding.blocked, Vec::<String>::new(), "{deciding:?}");

        // Damage only the known current job, then remove both current
        // references. The older terminal receipt remains.
        stop_process(supervisor);
        let state = fixture.job_directory(&job_id).join("state.json");
        std::fs::remove_file(&state).unwrap();
        assert!(process_alive(child));
        let task = fixture.task();
        let marker = format!(
            "super-agent.reviewer-resume.{}.{}.{}.final-completion",
            fixture.task_id, task.spec_version, task.review_round
        );
        let operation_pattern = format!(
            "super-agent.launch-op.{}.reviewer.resume.{}.{}.final-completion",
            fixture.task_id, task.spec_version, task.review_round
        );
        fixture
            .context
            .db
            .with_conn(|conn| -> Result<()> {
                conn.execute(
                    "DELETE FROM peripheral_settings WHERE key=?1",
                    [&marker],
                )?;
                conn.execute(
                    "DELETE FROM peripheral_settings WHERE key=?1",
                    [&operation_pattern],
                )?;
                Ok(())
            })
            .unwrap()
            .unwrap();
        assert!(fixture
            .context
            .setting::<Value>(&marker)
            .unwrap()
            .is_none());
        assert!(fixture
            .context
            .setting::<Value>(&operation_pattern)
            .unwrap()
            .is_none());

        let blocked = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(
            fixture.task_completed_count(),
            0,
            "older terminal receipts satisfied the current completion review: {blocked:?}"
        );
        assert!(
            blocked.blocked.iter().any(|reason| {
                reason.contains("no durable final-completion completion-review receipt")
                    && reason.contains(&fixture.task_id)
            }),
            "the missing current-stage receipt must be Unknown: {blocked:?}"
        );
        assert_eq!(fixture.task().status, Status::Blocked);
        assert!(process_alive(child));

        let reopened_context = crate::services::ServiceContext::open(
            RuntimePaths::new(fixture.data.clone()).unwrap(),
            Arc::new(crate::services::EnvironmentCredentials),
        )
        .unwrap();
        let reopened_store = TaskStore::new(reopened_context.db.clone(), fixture.data.clone());
        let reopened = Orchestrator::new(
            reopened_context,
            TwoRepoDeliveryFixture::config(),
            TwoRepoDeliveryFixture::provider(),
            "bypass".into(),
            Some(fixture.supervisor.clone()),
        );
        let _ = reopened.tick(&fixture.router).await.unwrap();
        assert_eq!(fixture.task_completed_count(), 0);
        assert_eq!(
            reopened_store
                .get_task(&fixture.task_id)
                .unwrap()
                .unwrap()
                .status,
            Status::Blocked
        );
        held.release();
    }

    /// A valid active job must wait, then complete once its real process is
    /// terminal. This is the positive control for the strict inspection.
    #[tokio::test]
    async fn a_live_completion_review_waits_then_completes_after_it_stops() {
        let fixture = TwoRepoDeliveryFixture::new();
        let plan = fixture.prepare_plan();
        fixture.deliver_all(&plan).await;
        let mut held = fixture.hold_future_children();
        let orchestrator = fixture.orchestrator();
        let launched = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(launched.blocked, Vec::<String>::new(), "{launched:?}");
        let (child, _supervisor) = held.wait_for_pids();
        fixture.final_reports(&plan, true);
        let deciding = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(deciding.blocked, Vec::<String>::new(), "{deciding:?}");

        let waiting = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(waiting.blocked, Vec::<String>::new(), "{waiting:?}");
        assert_eq!(fixture.task_completed_count(), 0);
        assert_eq!(fixture.task().status, Status::Running);
        assert!(process_alive(child));

        held.release();
        fixture.wait_settled().await;
        let completed = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(completed.blocked, Vec::<String>::new(), "{completed:?}");
        assert_eq!(fixture.task().status, Status::Completed);
        assert_eq!(fixture.task_completed_count(), 1);
    }

    /// Rewrites one ownership field of the exact held job while its supervisor
    /// is frozen. The job status itself stays `running`; only identity is
    /// wrong, so the completion check must block rather than wait or accept it.
    async fn assert_tampered_completion_job_blocks(field: &str, needle: &str) {
        let fixture = TwoRepoDeliveryFixture::new();
        let plan = fixture.prepare_plan();
        fixture.deliver_all(&plan).await;
        let mut held = fixture.hold_future_children();
        let orchestrator = fixture.orchestrator();
        let launched = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(launched.blocked, Vec::<String>::new(), "{launched:?}");
        let (child, supervisor) = held.wait_for_pids();
        let job_id = fixture.final_completion_marker()["job_id"]
            .as_str()
            .expect("completion review job")
            .to_owned();
        fixture.final_reports(&plan, true);
        let deciding = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(deciding.blocked, Vec::<String>::new(), "{deciding:?}");

        stop_process(supervisor);
        let state_path = fixture.job_directory(&job_id).join("state.json");
        let mut state: Value =
            serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
        match field {
            "id" => state["id"] = json!(uuid::Uuid::new_v4().to_string()),
            "session" => state["session_id"] = json!("other-session"),
            "parent" => state["parent_session_id"] = json!("other-parent"),
            other => panic!("unknown tamper field {other}"),
        }
        std::fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
        assert!(process_alive(child));

        let blocked = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(
            fixture.task_completed_count(),
            0,
            "a {field} mismatch was treated as a stopped job: {blocked:?}"
        );
        assert!(
            blocked
                .blocked
                .iter()
                .any(|reason| reason.contains(&job_id) && reason.contains(needle)),
            "the {field} mismatch must block with its exact diagnosis: {blocked:?}"
        );
        assert_eq!(fixture.task().status, Status::Blocked);
        held.release();
    }

    #[tokio::test]
    async fn a_completion_review_with_wrong_job_identity_never_completes() {
        assert_tampered_completion_job_blocks("id", "identity").await;
        assert_tampered_completion_job_blocks("session", "child session").await;
        assert_tampered_completion_job_blocks("parent", "parent session").await;
    }

    /// A terminal delivery Coder job is real evidence. If its known durable
    /// state becomes uncertain after the targets were delivered, completion
    /// blocks instead of treating the delivery receipt as proof of exit.
    #[tokio::test]
    async fn a_delivery_coder_receipt_whose_state_is_damaged_blocks_completion() {
        let fixture = TwoRepoDeliveryFixture::new();
        let plan = fixture.prepare_plan();
        fixture.deliver_all(&plan).await;
        let target = fixture.target_plan(&plan, 0).clone();
        let delivery = fixture
            .store()
            .repository_dispatch(&fixture.task_id, &plan.operation_id, &target.repository_id)
            .unwrap()
            .expect("the moved repository has a durable delivery receipt");
        let job_id = delivery.job_id.clone().expect("the receipt names its job");

        let orchestrator = fixture.orchestrator();
        let _ = orchestrator.tick(&fixture.router).await.unwrap();
        fixture.wait_settled().await;
        fixture.final_reports(&plan, true);
        let _ = orchestrator.tick(&fixture.router).await.unwrap();

        let state = fixture.job_directory(&job_id).join("state.json");
        assert!(state.exists(), "the delivery job must have been terminal");
        std::fs::remove_file(&state).unwrap();
        let blocked = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(
            fixture.task_completed_count(),
            0,
            "a damaged delivery Coder receipt was treated as stopped: {blocked:?}"
        );
        assert!(
            blocked.blocked.iter().any(|reason| {
                reason.contains(&job_id) && reason.contains("no durable task state")
            }),
            "the delivery receipt must block with its job identity: {blocked:?}"
        );
        assert_eq!(fixture.task().status, Status::Blocked);
        assert!(fixture.observation(&plan, 0) && fixture.observation(&plan, 1));
        assert_eq!(
            git(
                &fixture.repos[0].root,
                &[
                    "rev-parse",
                    &format!("refs/heads/{}", fixture.repos[0].branch)
                ]
            ),
            fixture.repos[0].reviewed
        );
    }

    /// The Coder side must prove the *current* plan's delivery jobs stopped.
    /// Reports, delivery observations and older terminal receipts survive, but
    /// the current plan's Coder delivery operations and receipt references are
    /// removed. Absence of the current receipts is not proof of process exit.
    #[tokio::test]
    async fn a_missing_current_coder_delivery_receipt_is_not_treated_as_stopped() {
        let fixture = TwoRepoDeliveryFixture::new();
        let plan = fixture.prepare_plan();
        // Every planned repository has to move, so delivery dispatches the real
        // Coder and leaves one durable receipt per repository.
        fixture.deliver_all(&plan).await;
        assert_eq!(
            fixture
                .store()
                .delivery_dispatches(&fixture.task_id)
                .unwrap()
                .len(),
            2,
            "both moved repositories must have a real delivery receipt"
        );

        let orchestrator = fixture.orchestrator();
        // The reports do not exist yet, so this tick resumes the same Reviewer
        // for the final completion review and records its durable receipt.
        let launched = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(launched.blocked, Vec::<String>::new(), "{launched:?}");
        fixture.wait_settled().await;
        fixture.final_reports(&plan, true);
        let deciding = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(deciding.blocked, Vec::<String>::new(), "{deciding:?}");

        // Keep every report, observation and older terminal receipt; remove
        // only the current plan's Coder delivery operations and receipts.
        fixture
            .context
            .db
            .with_conn(|conn| -> Result<()> {
                conn.execute(
                    "DELETE FROM sa_repository_dispatches WHERE task_id=?1 AND plan_operation_id=?2",
                    rusqlite::params![fixture.task_id, plan.operation_id],
                )?;
                conn.execute(
                    "DELETE FROM sa_delivery_dispatches WHERE task_id=?1 AND plan_operation_id=?2",
                    rusqlite::params![fixture.task_id, plan.operation_id],
                )?;
                conn.execute(
                    "DELETE FROM peripheral_settings WHERE key LIKE ?1",
                    [format!("super-agent.launch-op.{}.coder.%", fixture.task_id)],
                )?;
                Ok(())
            })
            .unwrap()
            .unwrap();

        let blocked = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(
            fixture.task_completed_count(),
            0,
            "removing the current Coder delivery receipts was treated as proof they stopped: {blocked:?}"
        );
        assert!(
            blocked
                .blocked
                .iter()
                .any(|reason| reason.contains(&plan.operation_id) && reason.contains("Coder")),
            "the missing current Coder receipt must block with its plan identity: {blocked:?}"
        );
        assert_eq!(fixture.task().status, Status::Blocked);
        // A reopened host reads the same durable state and keeps the block.
        let reopened_context = crate::services::ServiceContext::open(
            RuntimePaths::new(fixture.data.clone()).unwrap(),
            Arc::new(crate::services::EnvironmentCredentials),
        )
        .unwrap();
        let reopened_store = TaskStore::new(reopened_context.db.clone(), fixture.data.clone());
        let reopened = Orchestrator::new(
            reopened_context,
            TwoRepoDeliveryFixture::config(),
            TwoRepoDeliveryFixture::provider(),
            "bypass".into(),
            Some(fixture.supervisor.clone()),
        );
        let reopened_report = reopened.tick(&fixture.router).await.unwrap();
        assert_eq!(
            fixture.task_completed_count(),
            0,
            "a reopened host completed without the current Coder receipt: {reopened_report:?}"
        );
        assert_eq!(
            reopened_store
                .get_task(&fixture.task_id)
                .unwrap()
                .unwrap()
                .status,
            Status::Blocked
        );
        // The delivery itself is untouched: observations and target OIDs stay.
        assert!(fixture.observation(&plan, 0) && fixture.observation(&plan, 1));
        assert_eq!(
            git(
                &fixture.repos[0].root,
                &["rev-parse", &format!("refs/heads/{}", fixture.repos[0].branch)]
            ),
            fixture.repos[0].reviewed
        );
    }

    /// The `final-completion` stage key is shared by every plan of one
    /// spec/round. A receipt that names a different persisted plan must not
    /// prove the current plan's Reviewer job stopped.
    #[tokio::test]
    async fn a_completion_review_from_another_plan_never_completes() {
        let fixture = TwoRepoDeliveryFixture::new();
        let plan = fixture.prepare_plan();
        fixture.deliver_all(&plan).await;
        let orchestrator = fixture.orchestrator();
        let launched = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(launched.blocked, Vec::<String>::new(), "{launched:?}");
        fixture.wait_settled().await;
        fixture.final_reports(&plan, true);
        let deciding = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(deciding.blocked, Vec::<String>::new(), "{deciding:?}");

        // A second, genuinely persisted plan for the same spec/round. It is
        // older, so the plan above stays authoritative.
        let mut other = plan.clone();
        other.operation_id = "fixture-other-plan".into();
        other.created_at = "2020-01-01T00:00:00Z".into();
        fixture.store().save_delivery_plan(&other).unwrap();

        let task = fixture.task();
        let key = format!(
            "super-agent.reviewer-resume.{}.{}.{}.final-completion",
            fixture.task_id, task.spec_version, task.review_round
        );
        let marker = fixture.final_completion_marker();
        fixture
            .context
            .save_setting(
                &key,
                &json!({
                    "session_id": marker["session_id"],
                    "job_id": marker["job_id"],
                    "plan_operation_id": other.operation_id,
                    "at": "2026-01-01T00:00:00Z"
                }),
            )
            .unwrap();

        let blocked = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(
            fixture.task_completed_count(),
            0,
            "a review receipt from another plan was treated as the current one: {blocked:?}"
        );
        assert!(
            blocked
                .blocked
                .iter()
                .any(|reason| reason.contains(&plan.operation_id)
                    && reason.contains("final-completion")),
            "the foreign-plan receipt must block with the current plan identity: {blocked:?}"
        );
        assert_eq!(fixture.task().status, Status::Blocked);
    }

    /// The plan can record a real baseline to move from, and an external
    /// process can still move the real target to the approved commit before any
    /// dispatch. The host then observes that target without starting a child,
    /// and completion must accept that durable no-dispatch observation instead
    /// of demanding a Coder receipt.
    #[tokio::test]
    async fn a_target_moved_to_the_approved_commit_before_dispatch_completes_without_a_receipt() {
        let fixture = TwoRepoDeliveryFixture::new();
        let plan = fixture.prepare_plan();
        for target in &plan.repositories {
            assert_ne!(
                target.old_oid, target.new_oid,
                "the plan records a real baseline to move from"
            );
        }
        // The targets reach the approved commits without any delivery child.
        fixture.set_target(0, &fixture.repos[0].reviewed);
        fixture.set_target(1, &fixture.repos[1].reviewed);
        let orchestrator = fixture.orchestrator();
        for _ in 0..2 {
            let tick = orchestrator.tick(&fixture.router).await.unwrap();
            assert_eq!(tick.blocked, Vec::<String>::new(), "{tick:?}");
        }
        assert!(
            fixture
                .store()
                .delivery_dispatches(&fixture.task_id)
                .unwrap()
                .is_empty(),
            "no delivery child may be dispatched for a target already at the approved commit"
        );
        for target in &plan.repositories {
            let observation = fixture
                .store()
                .repository_delivery_observation(
                    &fixture.task_id,
                    &plan.operation_id,
                    &target.repository_id,
                )
                .unwrap()
                .expect("the host verified this target");
            assert_eq!(
                observation.source,
                crate::super_agent::delivery::DeliveryObservationSource::HostVerifiedAlreadyAtTarget
            );
        }
        assert_eq!(fixture.task().phase, Phase::Finalizing);

        let launched = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(launched.blocked, Vec::<String>::new(), "{launched:?}");
        fixture.wait_settled().await;
        fixture.final_reports(&plan, true);
        let deciding = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(deciding.blocked, Vec::<String>::new(), "{deciding:?}");
        let completed = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(completed.blocked, Vec::<String>::new(), "{completed:?}");
        assert_eq!(fixture.task().status, Status::Completed);
        assert_eq!(fixture.task_completed_count(), 1);

        // A reopened host reads the same durable no-dispatch provenance and
        // keeps the task completed instead of demanding a receipt.
        let reopened_context = crate::services::ServiceContext::open(
            RuntimePaths::new(fixture.data.clone()).unwrap(),
            Arc::new(crate::services::EnvironmentCredentials),
        )
        .unwrap();
        let reopened_store = TaskStore::new(reopened_context.db.clone(), fixture.data.clone());
        let reopened = Orchestrator::new(
            reopened_context,
            TwoRepoDeliveryFixture::config(),
            TwoRepoDeliveryFixture::provider(),
            "bypass".into(),
            Some(fixture.supervisor.clone()),
        );
        let reopened_report = reopened.tick(&fixture.router).await.unwrap();
        assert_eq!(
            reopened_report.blocked,
            Vec::<String>::new(),
            "{reopened_report:?}"
        );
        assert_eq!(
            reopened_store
                .get_task(&fixture.task_id)
                .unwrap()
                .unwrap()
                .status,
            Status::Completed
        );
        assert_eq!(fixture.task_completed_count(), 1);
    }

    /// A no-dispatch observation belongs to the plan that produced it. A row
    /// from another plan cannot stand in for the current plan's repository.
    #[tokio::test]
    async fn a_no_dispatch_observation_from_another_plan_never_covers_the_current_plan() {
        let fixture = TwoRepoDeliveryFixture::new();
        let plan = fixture.prepare_plan();
        fixture.set_target(0, &fixture.repos[0].reviewed);
        fixture.set_target(1, &fixture.repos[1].reviewed);
        let orchestrator = fixture.orchestrator();
        for _ in 0..2 {
            let tick = orchestrator.tick(&fixture.router).await.unwrap();
            assert_eq!(tick.blocked, Vec::<String>::new(), "{tick:?}");
        }
        assert_eq!(fixture.task().phase, Phase::Finalizing);

        // A second persisted plan of the same spec/round, and the real
        // observations are re-attributed to it.
        let mut other = plan.clone();
        other.operation_id = "fixture-other-plan".into();
        other.created_at = "2020-01-01T00:00:00Z".into();
        fixture.store().save_delivery_plan(&other).unwrap();
        fixture
            .context
            .db
            .with_conn(|conn| -> Result<()> {
                conn.execute(
                    "UPDATE sa_repository_delivery_observations SET plan_operation_id=?2 WHERE task_id=?1",
                    rusqlite::params![fixture.task_id, other.operation_id],
                )?;
                Ok(())
            })
            .unwrap()
            .unwrap();

        fixture.final_reports(&plan, true);
        for _ in 0..3 {
            let _ = orchestrator.tick(&fixture.router).await.unwrap();
        }
        assert_eq!(
            fixture.task_completed_count(),
            0,
            "an observation from another plan covered the current one"
        );
        assert_ne!(fixture.task().status, Status::Completed);
    }

    /// The required completion-review receipt is the stage the persisted plan's
    /// decision actually identifies: a changed tree reuses its pre-delivery
    /// `final-delivery` review, an unchanged tree requires a post-delivery
    /// `final-completion` review.
    #[test]
    fn the_required_completion_review_stage_is_derived_from_the_plan_decision() {
        let fixture = TwoRepoDeliveryFixture::new();
        let plan = fixture.prepare_plan();
        let events = fixture.store().events(&fixture.task_id).unwrap();
        assert_eq!(
            required_completion_reviewer_purpose(&events, &plan),
            Some("final-completion")
        );
        let mut changed_plan = plan.clone();
        changed_plan.review_event_id = "changed-final-decision".into();
        let mut decision = events
            .iter()
            .find(|event| event.event_type == EventType::ReviewCompleted)
            .unwrap()
            .clone();
        decision.event_id = "changed-final-decision".into();
        decision.payload["stage"] = json!(ReviewStage::FinalDelivery.as_str());
        assert_eq!(
            required_completion_reviewer_purpose(std::slice::from_ref(&decision), &changed_plan),
            Some("final-delivery")
        );
        let mut foreign = changed_plan.clone();
        foreign.review_round += 1;
        assert_eq!(
            required_completion_reviewer_purpose(&[decision], &foreign),
            None,
            "a decision from another round cannot define the required stage"
        );
    }

    /// A task with no member receipt at all cannot prove process exit. It must
    /// block with a compatibility reason instead of inferring "stopped" from
    /// absence, and it must not need any fabricated receipt.
    #[tokio::test]
    async fn a_completion_without_any_member_receipt_is_not_treated_as_stopped() {
        let fixture = TwoRepoDeliveryFixture::new();
        let plan = fixture.prepare_plan();
        fixture.deliver_all(&plan).await;
        fixture.final_reports(&plan, true);
        let orchestrator = fixture.orchestrator();
        let deciding = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(deciding.blocked, Vec::<String>::new(), "{deciding:?}");

        let blocked = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(fixture.task_completed_count(), 0);
        assert!(
            blocked
                .blocked
                .iter()
                .any(|reason| reason.contains(
                    "no durable final-completion completion-review receipt exists"
                )),
            "absence of every receipt must block instead of concluding stopped: {blocked:?}"
        );
        assert_eq!(fixture.task().status, Status::Blocked);
    }

    /// Damage confined to an unrelated background record must not block a task
    /// whose own known jobs are all terminal.
    #[tokio::test]
    async fn an_unrelated_corrupt_job_does_not_block_a_valid_completion() {
        let fixture = TwoRepoDeliveryFixture::new();
        let plan = fixture.prepare_plan();
        fixture.deliver_all(&plan).await;
        let orchestrator = fixture.orchestrator();
        let _ = orchestrator.tick(&fixture.router).await.unwrap();
        fixture.wait_settled().await;
        fixture.final_reports(&plan, true);
        let _ = orchestrator.tick(&fixture.router).await.unwrap();

        let foreign = uuid::Uuid::new_v4().to_string();
        let directory =
            crate::services::background_sessions::directory(&fixture.data, &foreign).unwrap();
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("state.json"), b"{not json").unwrap();

        let completed = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(completed.blocked, Vec::<String>::new(), "{completed:?}");
        assert_eq!(fixture.task().status, Status::Completed);
        assert_eq!(fixture.task_completed_count(), 1);
        assert!(directory.join("state.json").exists());
    }

    #[tokio::test]
    async fn a_target_moved_after_delivery_blocks_completion() {
        let fixture = TwoRepoDeliveryFixture::new();
        let plan = fixture.prepare_plan();
        fixture.deliver_all(&plan).await;
        fixture.final_reports(&plan, true);
        // An external process moves B after the host recorded the delivery.
        let elsewhere = fixture.move_target_elsewhere(1);
        let orchestrator = fixture.orchestrator();

        let report = orchestrator.tick(&fixture.router).await.unwrap();
        assert!(
            report.blocked.iter().any(|reason| {
                reason.contains("completion") && reason.contains(&fixture.repos[1].repository_id)
            }),
            "a stale delivered target must block completion: {report:?}"
        );
        let task = fixture.task();
        assert_eq!(task.status, Status::Blocked);
        assert_eq!(task.phase, Phase::Finalizing);
        assert_eq!(fixture.task_completed_count(), 0);
        assert_eq!(
            git(
                &fixture.repos[1].root,
                &[
                    "rev-parse",
                    &format!("refs/heads/{}", fixture.repos[1].branch)
                ]
            ),
            elsewhere,
            "a failed completion check must not rewind or rewrite the target"
        );
        assert_eq!(
            git(
                &fixture.repos[0].root,
                &[
                    "rev-parse",
                    &format!("refs/heads/{}", fixture.repos[0].branch)
                ]
            ),
            fixture.repos[0].reviewed
        );
        assert!(fixture.observation(&plan, 0) && fixture.observation(&plan, 1));
    }

    #[tokio::test]
    async fn a_rejected_finalization_review_blocks_without_rollback() {
        let fixture = TwoRepoDeliveryFixture::new();
        let plan = fixture.prepare_plan();
        fixture.deliver_all(&plan).await;
        fixture.final_reports(&plan, false);
        let orchestrator = fixture.orchestrator();

        let report = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(report.blocked, Vec::<String>::new(), "{report:?}");
        let task = fixture.task();
        assert_eq!(task.status, Status::Blocked);
        assert_eq!(task.phase, Phase::Finalizing);
        assert_eq!(fixture.task_completed_count(), 0);
        // Delivery is not rolled back: the targets keep the delivered commits
        // and the per-repository progress stays Completed.
        for (index, repo) in task.repositories.iter().enumerate() {
            assert_eq!(
                repo.integration_status,
                crate::super_agent::IntegrationStatus::Completed
            );
            assert_eq!(
                git(
                    &fixture.repos[index].root,
                    &[
                        "rev-parse",
                        &format!("refs/heads/{}", fixture.repos[index].branch)
                    ]
                ),
                fixture.repos[index].reviewed
            );
        }
        // A later tick cannot complete a blocked task.
        let _ = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(fixture.task_completed_count(), 0);
    }

    #[tokio::test]
    async fn a_non_git_only_finalizing_task_is_not_completed() {
        let fixture = TwoRepoDeliveryFixture::new();
        // Make the task non-Git-only, as the state machine allows after an
        // approved FINAL_DELIVERY review with no Git targets.
        fixture
            .store()
            .db()
            .with_conn(|conn| {
                conn.execute(
                    "DELETE FROM sa_task_repos WHERE task_id=?1",
                    [&fixture.task_id],
                )
            })
            .unwrap()
            .unwrap();
        fixture
            .store()
            .save_non_git_target(&crate::super_agent::NonGitTarget {
                task_id: fixture.task_id.clone(),
                path: fixture.data.join("scripts"),
                snapshot_dir: fixture.data.join("snapshots"),
                snapshot_manifest_json: "{}".into(),
                diff_ref: None,
                status: crate::super_agent::NonGitStatus::Prepared,
                lock_key: "path:fixture".into(),
                submission: None,
            })
            .unwrap();
        fixture
            .store()
            .db()
            .with_conn(|conn| {
                conn.execute(
                    "UPDATE sa_tasks SET phase='FINALIZING', status='RUNNING' WHERE task_id=?1",
                    [&fixture.task_id],
                )
            })
            .unwrap()
            .unwrap();
        let orchestrator = fixture.orchestrator();

        let report = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(report.blocked, Vec::<String>::new(), "{report:?}");
        let task = fixture.task();
        assert_eq!(task.phase, Phase::Finalizing);
        assert_eq!(task.status, Status::Running);
        assert_eq!(fixture.task_completed_count(), 0);
        assert_eq!(
            task.non_git_targets[0].status,
            crate::super_agent::NonGitStatus::Prepared,
            "a bounded Git-only finalizer must not touch non-Git progress"
        );
    }

    #[tokio::test]
    async fn a_completion_review_that_stops_without_reports_blocks_once_and_stays_blocked() {
        let fixture = TwoRepoDeliveryFixture::new();
        let plan = fixture.prepare_plan();
        fixture.deliver_all(&plan).await;
        let orchestrator = fixture.orchestrator();

        // The same Reviewer is resumed once, then exits without submitting.
        let first = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(first.blocked, Vec::<String>::new(), "{first:?}");
        fixture.wait_settled().await;
        let marker = fixture.final_completion_marker();
        let job_id = marker["job_id"]
            .as_str()
            .expect("completion review job")
            .to_owned();
        let second = orchestrator.tick(&fixture.router).await.unwrap();
        assert!(
            second.blocked.iter().any(|reason| {
                reason.contains(&job_id)
                    && reason.contains("without a valid FINAL_DELIVERY review and harness")
                    && reason.contains("review present: false")
                    && reason.contains("harness present: false")
            }),
            "a stopped review without reports must block with its diagnosis: {second:?}"
        );
        let task = fixture.task();
        assert_eq!(task.status, Status::Blocked);
        assert_eq!(task.phase, Phase::Finalizing);
        assert_eq!(fixture.task_completed_count(), 0);

        // Repeated and reopened ticks neither resume again nor duplicate work.
        let jobs = crate::services::background_sessions::list(&fixture.data, Some(&fixture.router))
            .unwrap_or_default()
            .len();
        let repeated = orchestrator.tick(&fixture.router).await.unwrap();
        assert_eq!(repeated.blocked, Vec::<String>::new(), "{repeated:?}");
        assert_eq!(
            crate::services::background_sessions::list(&fixture.data, Some(&fixture.router))
                .unwrap_or_default()
                .len(),
            jobs,
            "a blocked completion review launched another child"
        );
        let reopened_context = crate::services::ServiceContext::open(
            RuntimePaths::new(fixture.data.clone()).unwrap(),
            Arc::new(crate::services::EnvironmentCredentials),
        )
        .unwrap();
        let reopened_store = TaskStore::new(reopened_context.db.clone(), fixture.data.clone());
        let reopened = Orchestrator::new(
            reopened_context,
            TwoRepoDeliveryFixture::config(),
            TwoRepoDeliveryFixture::provider(),
            "bypass".into(),
            Some(fixture.supervisor.clone()),
        );
        let reopened_report = reopened.tick(&fixture.router).await.unwrap();
        assert_eq!(
            reopened_report.blocked,
            Vec::<String>::new(),
            "{reopened_report:?}"
        );
        assert_eq!(
            crate::services::background_sessions::list(&fixture.data, Some(&fixture.router))
                .unwrap_or_default()
                .len(),
            jobs
        );
        assert_eq!(
            reopened_store
                .get_task(&fixture.task_id)
                .unwrap()
                .unwrap()
                .status,
            Status::Blocked
        );
        assert_eq!(
            fixture.final_completion_marker()["job_id"],
            marker["job_id"],
            "the durable completion-review job was replaced"
        );
    }

    #[tokio::test]
    async fn a_single_completion_report_blocks_with_its_missing_evidence() {
        let fixture = TwoRepoDeliveryFixture::new();
        let plan = fixture.prepare_plan();
        fixture.deliver_all(&plan).await;
        let orchestrator = fixture.orchestrator();
        let _ = orchestrator.tick(&fixture.router).await.unwrap();
        fixture.wait_settled().await;
        fixture.final_review_report(&plan, true);

        let report = orchestrator.tick(&fixture.router).await.unwrap();
        assert!(
            report.blocked.iter().any(|reason| {
                reason.contains("without a valid FINAL_DELIVERY review and harness")
                    && reason.contains("review present: true")
                    && reason.contains("harness present: false")
            }),
            "one report must not be treated as complete evidence: {report:?}"
        );
        assert_eq!(fixture.task().status, Status::Blocked);
        assert_eq!(fixture.task_completed_count(), 0);
    }

    #[tokio::test]
    async fn a_failed_or_missing_completion_reviewer_blocks_without_a_new_resume() {
        // The Reviewer child exits non-zero.
        {
            let fixture = TwoRepoDeliveryFixture::new();
            let plan = fixture.prepare_plan();
            fixture.deliver_all(&plan).await;
            fixture.fail_future_children(7);
            let orchestrator = fixture.orchestrator();
            let _ = orchestrator.tick(&fixture.router).await.unwrap();
            fixture.wait_settled().await;
            let jobs =
                crate::services::background_sessions::list(&fixture.data, Some(&fixture.router))
                    .unwrap_or_default()
                    .len();

            let report = orchestrator.tick(&fixture.router).await.unwrap();
            assert!(
                report
                    .blocked
                    .iter()
                    .any(|reason| reason.contains("ended failed")),
                "a failed completion reviewer must block: {report:?}"
            );
            assert_eq!(fixture.task().status, Status::Blocked);
            assert_eq!(
                crate::services::background_sessions::list(&fixture.data, Some(&fixture.router))
                    .unwrap_or_default()
                    .len(),
                jobs,
                "a failed completion reviewer must not be resumed again"
            );
            let _ = orchestrator.tick(&fixture.router).await.unwrap();
            assert_eq!(
                crate::services::background_sessions::list(&fixture.data, Some(&fixture.router))
                    .unwrap_or_default()
                    .len(),
                jobs
            );
        }

        // The reserved job produced no durable state at all.
        {
            let fixture = TwoRepoDeliveryFixture::new();
            let plan = fixture.prepare_plan();
            fixture.deliver_all(&plan).await;
            let orchestrator = fixture.orchestrator();
            let _ = orchestrator.tick(&fixture.router).await.unwrap();
            fixture.wait_settled().await;
            let job_id = fixture.final_completion_marker()["job_id"]
                .as_str()
                .expect("completion review job")
                .to_owned();
            let directory =
                crate::services::background_sessions::directory(&fixture.data, &job_id).unwrap();
            std::fs::remove_dir_all(&directory).unwrap();

            let report = orchestrator.tick(&fixture.router).await.unwrap();
            assert!(
                report
                    .blocked
                    .iter()
                    .any(|reason| reason.contains("produced no durable state")),
                "a missing completion job must block: {report:?}"
            );
            assert_eq!(fixture.task().status, Status::Blocked);
            assert_eq!(fixture.task_completed_count(), 0);
        }
    }

    #[tokio::test]
    async fn a_completion_review_marker_is_never_adopted_for_another_session() {
        let fixture = TwoRepoDeliveryFixture::new();
        let plan = fixture.prepare_plan();
        fixture.deliver_all(&plan).await;
        let orchestrator = fixture.orchestrator();
        let _ = orchestrator.tick(&fixture.router).await.unwrap();
        fixture.wait_settled().await;
        let reviewer = fixture
            .task()
            .reviewer
            .session_id
            .clone()
            .expect("reviewer session");
        let key = format!(
            "super-agent.reviewer-resume.{}.{}.{}.final-completion",
            fixture.task_id, 1, 0
        );

        // The marker names this task's Reviewer but points at another
        // session's still-running job. Ownership is checked before status, so
        // this can never wait on, adopt or resume unrelated work.
        let other = uuid::Uuid::new_v4().to_string();
        let directory =
            crate::services::background_sessions::directory(&fixture.data, &other).unwrap();
        std::fs::create_dir_all(&directory).unwrap();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let fake = crate::services::background_sessions::Job {
            id: other.clone(),
            foreground: false,
            parent_session_id: "other-parent".into(),
            session_id: "other-session".into(),
            title: "unrelated".into(),
            status: "running".into(),
            supervisor_pid: None,
            child_pid: Some(1),
            started_ms: now,
            heartbeat_ms: now,
            finished_ms: None,
            exit_code: None,
            error: None,
            launched: true,
        };
        std::fs::write(
            directory.join("state.json"),
            serde_json::to_vec(&fake).unwrap(),
        )
        .unwrap();
        fixture
            .context
            .save_setting(
                &key,
                &json!({"session_id": reviewer, "job_id": other, "at": "2026-01-01T00:00:00Z"}),
            )
            .unwrap();
        let report = orchestrator.tick(&fixture.router).await.unwrap();
        assert!(
            report.blocked.iter().any(|reason| {
                reason.contains("belongs to session other-session")
                    && reason.contains("not this Reviewer")
            }),
            "an unrelated active job must block, not wait: {report:?}"
        );
        assert_eq!(fixture.task().status, Status::Blocked);
        assert_eq!(fixture.task_completed_count(), 0);

        // The marker's own session must be this task's Reviewer as well.
        let fixture = TwoRepoDeliveryFixture::new();
        let plan = fixture.prepare_plan();
        fixture.deliver_all(&plan).await;
        let orchestrator = fixture.orchestrator();
        let _ = orchestrator.tick(&fixture.router).await.unwrap();
        fixture.wait_settled().await;
        let marker = fixture.final_completion_marker();
        let key = format!(
            "super-agent.reviewer-resume.{}.{}.{}.final-completion",
            fixture.task_id, 1, 0
        );
        fixture
            .context
            .save_setting(
                &key,
                &json!({
                    "session_id": "other-session",
                    "job_id": marker["job_id"],
                    "at": "2026-01-01T00:00:00Z"
                }),
            )
            .unwrap();
        let report = orchestrator.tick(&fixture.router).await.unwrap();
        assert!(
            report.blocked.iter().any(|reason| {
                reason.contains("names session")
                    && reason.contains("other-session")
                    && reason.contains("not this task's Reviewer")
            }),
            "a marker for another session must block: {report:?}"
        );
        assert_eq!(fixture.task().status, Status::Blocked);
        assert_eq!(fixture.task_completed_count(), 0);
    }
    include!("cleanup_tests.rs");
}
