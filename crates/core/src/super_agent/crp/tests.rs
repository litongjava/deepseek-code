use super::*;
use crate::super_agent::role::{self, Role};
use crate::super_agent::store::TaskStore;
use serde_json::json;
use std::sync::Arc;

struct Fixture {
    _temp: tempfile::TempDir,
    data: PathBuf,
    db: Arc<DbManager>,
    runtime: ToolRuntime,
    router: String,
    coder: String,
    reviewer: String,
    other_router: String,
    outsider: String,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let router_dir = temp.path().join("router");
        let coder_dir = temp.path().join("coder");
        let reviewer_dir = temp.path().join("reviewer");
        let outsider_dir = temp.path().join("outsider");
        for path in [&router_dir, &coder_dir, &reviewer_dir, &outsider_dir] {
            std::fs::create_dir_all(path).unwrap();
        }
        let context = ServiceContext::open(
            crate::runtime::RuntimePaths::new(data.clone()).unwrap(),
            Arc::new(EnvironmentCredentials),
        )
        .unwrap();
        let sessions = crate::runtime::SessionStore::new(context.db.clone());
        let router = sessions.create(&router_dir, "fixture", "fixture").unwrap();
        let other_router_dir = temp.path().join("other-router");
        std::fs::create_dir_all(&other_router_dir).unwrap();
        let other_router = sessions
            .create(&other_router_dir, "fixture", "fixture")
            .unwrap();
        let coder = sessions.create(&coder_dir, "fixture", "fixture").unwrap();
        let reviewer = sessions
            .create(&reviewer_dir, "fixture", "fixture")
            .unwrap();
        let outsider = sessions
            .create(&outsider_dir, "fixture", "fixture")
            .unwrap();
        role::set(&context.db, &router, Role::Router).unwrap();
        role::set(&context.db, &other_router, Role::Router).unwrap();
        let runtime = ToolRuntime {
            db: context.db.clone(),
            data_dir: data.clone(),
            session_id: router.clone(),
        };
        Self {
            _temp: temp,
            data,
            db: context.db.clone(),
            runtime,
            router,
            coder,
            reviewer,
            other_router,
            outsider,
        }
    }

    fn store(&self) -> TaskStore {
        TaskStore::new(self.db.clone(), self.data.clone())
    }

    fn spec(&self) -> Value {
        json!({
            "title": "Fixture task",
            "workspace": self._temp.path().to_string_lossy(),
            "targets": [self._temp.path().to_string_lossy()],
            "goal": "Do the work",
            "requirements": ["Keep behavior"],
            "acceptance_criteria": [{"id":"AC-001","description":"Tests pass"}],
            "constraints": [],
            "out_of_scope": [],
            "target_branch": null
        })
    }

    fn task(&self) -> TaskRecord {
        let value = execute(
            &self.runtime,
            "CrpCreate",
            json!({"spec": self.spec()}),
            "create-1",
        )
        .unwrap();
        let task_id = value["task_id"].as_str().unwrap().to_owned();
        self.store()
            .set_sessions(&task_id, Some(&self.coder), Some(&self.reviewer))
            .unwrap();
        self.store().get_task(&task_id).unwrap().unwrap()
    }

    fn runtime_for(&self, session: &str) -> ToolRuntime {
        ToolRuntime {
            db: self.runtime.db.clone(),
            data_dir: self.runtime.data_dir.clone(),
            session_id: session.into(),
        }
    }

    fn member(&self, role_name: &str) -> ToolRuntime {
        let session = if role_name == "coder" {
            &self.coder
        } else {
            &self.reviewer
        };
        self.runtime_for(session)
    }
}

fn plan_body(version: u32, round: u32) -> Value {
    json!({
        "spec_version": version,
        "round": round,
        "understanding": "Fixtures",
        "scope": ["tests"],
        "steps": ["write tests"],
        "verification": ["cargo test"],
        "risks": []
    })
}

#[test]
fn only_a_router_can_use_router_tools_and_scope_is_per_router() {
    let fixture = Fixture::new();
    let agent = fixture.runtime_for(&fixture.outsider);
    let error = execute(&agent, "CrpCreate", json!({"spec": fixture.spec()}), "x").unwrap_err();
    assert!(
        error.to_string().contains("requires a Router session"),
        "{error:#}"
    );

    let task = fixture.task();
    // Another Router cannot read or modify this task.
    let other = fixture.runtime_for(&fixture.other_router);
    for (tool, input) in [
        ("CrpShow", json!({"task_id": task.task_id})),
        ("CrpList", json!({})),
        (
            "CrpUpdateSpec",
            json!({"task_id": task.task_id, "spec": fixture.spec()}),
        ),
    ] {
        let result = execute(&other, tool, input, "other-router");
        if tool == "CrpList" {
            let value = result.unwrap();
            assert_eq!(value["count"], 0, "another Router saw foreign tasks");
            continue;
        }
        let error = result.unwrap_err();
        assert!(
            error.to_string().contains("another Router"),
            "{tool}: {error:#}"
        );
    }
    // A member session cannot use Router tools either.
    let error = execute(
        &fixture.member("coder"),
        "CrpShow",
        json!({"task_id": task.task_id}),
        "member-router-tool",
    )
    .unwrap_err();
    assert!(error.to_string().contains("requires a Router session"));
}

#[test]
fn create_is_idempotent_per_tool_call_and_lists_typed_summaries() {
    let fixture = Fixture::new();
    let first = execute(
        &fixture.runtime,
        "CrpCreate",
        json!({"spec": fixture.spec()}),
        "same-create",
    )
    .unwrap();
    let replay = execute(
        &fixture.runtime,
        "CrpCreate",
        json!({"spec": fixture.spec()}),
        "same-create",
    )
    .unwrap();
    assert_eq!(first["task_id"], replay["task_id"]);
    let mut changed = fixture.spec();
    changed["title"] = json!("Different task");
    let conflict = execute(
        &fixture.runtime,
        "CrpCreate",
        json!({"spec": changed}),
        "same-create",
    );
    assert!(conflict.is_err());

    let list = execute(&fixture.runtime, "CrpList", json!({}), "list").unwrap();
    assert_eq!(list["count"], 1);
    assert_eq!(list["tasks"][0]["title"], "Fixture task");
    let filtered = execute(
        &fixture.runtime,
        "CrpList",
        json!({"status": "completed"}),
        "list-completed",
    )
    .unwrap();
    assert_eq!(filtered["count"], 0);
    assert!(execute(
        &fixture.runtime,
        "CrpList",
        json!({"status": "nonsense"}),
        "list-bad"
    )
    .is_err());
}

#[test]
fn member_tools_require_current_membership_and_the_correct_kind() {
    let fixture = Fixture::new();
    let task = fixture.task();

    let outsider = execute(
        &fixture.runtime_for(&fixture.outsider),
        "CrpSubmit",
        json!({"kind":"implementation","payload":{"spec_version":1,"summary":"x"}}),
        "outsider",
    )
    .unwrap_err();
    assert!(outsider
        .to_string()
        .contains("not a current Coder or Reviewer"));

    // Reviewer cannot submit a Coder-only report kind.
    let wrong = execute(
        &fixture.member("reviewer"),
        "CrpSubmit",
        json!({"kind":"implementation","payload":{"spec_version":1,"round":0,"summary":"x"}}),
        "reviewer-implementation",
    )
    .unwrap_err();
    assert!(wrong.to_string().contains("cannot submit"), "{wrong:#}");

    // Coder cannot submit review or harness.
    for kind in ["review", "harness"] {
        let error = execute(
            &fixture.member("coder"),
            "CrpSubmit",
            json!({"kind":kind,"payload":{"spec_version":1,"round":0}}),
            &format!("coder-{kind}"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("cannot submit"));
    }

    // A typed body that does not match the envelope is rejected.
    let mismatched = execute(
        &fixture.member("coder"),
        "CrpSubmit",
        json!({"kind":"plan","payload": plan_body(2, 0)}),
        "coder-bad-version",
    )
    .unwrap_err();
    assert!(
        mismatched.to_string().contains("names version 2"),
        "{mismatched:#}"
    );

    // A valid plan is recorded as a report and a pending event, not applied.
    let submitted = execute(
        &fixture.member("coder"),
        "CrpSubmit",
        json!({"kind":"plan","payload": plan_body(task.spec_version, task.review_round)}),
        "coder-plan",
    )
    .unwrap();
    assert_eq!(submitted["kind"], "PLAN");
    assert_eq!(
        fixture
            .store()
            .get_task(&task.task_id)
            .unwrap()
            .unwrap()
            .phase,
        Phase::Discovering,
        "submit must not apply state"
    );
    // The same tool call is idempotent.
    let replay = execute(
        &fixture.member("coder"),
        "CrpSubmit",
        json!({"kind":"plan","payload": plan_body(task.spec_version, task.review_round)}),
        "coder-plan",
    )
    .unwrap();
    assert_eq!(submitted["report_id"], replay["report_id"]);
    assert_eq!(fixture.store().reports(&task.task_id).unwrap().len(), 1);
}

#[test]
fn approvals_must_exist_be_current_and_match_the_policy_version() {
    let fixture = Fixture::new();
    let policy = crate::super_agent::StatePolicy::default();
    let task = fixture.task();
    // Move to planning the way the host does.
    fixture
        .store()
        .apply_event(
            &task.task_id,
            NewEvent {
                session_id: None,
                run_id: None,
                event_type: EventType::DiscoveryCompleted,
                causation_id: None,
                operation_id: "discovery".into(),
                payload: json!({}),
            },
            &policy,
        )
        .unwrap();
    let requested = fixture
        .store()
        .apply_event(
            &task.task_id,
            NewEvent {
                session_id: None,
                run_id: None,
                event_type: EventType::ApprovalRequested,
                causation_id: None,
                operation_id: "approval-request".into(),
                payload: json!({"request_type":"PLAN"}),
            },
            &policy,
        )
        .unwrap();
    let _ = requested;
    let task = fixture.store().get_task(&task.task_id).unwrap().unwrap();
    assert_eq!(task.status, crate::super_agent::Status::WaitingUser);
    let approval = fixture
        .store()
        .request_approval(
            "request-plan",
            &task.task_id,
            crate::super_agent::store::ApprovalDraft {
                source_session_id: fixture.coder.clone(),
                parent_session_id: fixture.router.clone(),
                request_type: ApprovalType::Plan,
                request: json!({"plan":"x"}),
                task_spec_version: task.spec_version,
                permission_policy_version: task.permission_policy_version,
            },
        )
        .unwrap();

    // Another Router cannot answer it.
    let error = execute(
        &fixture.runtime_for(&fixture.other_router),
        "CrpAnswer",
        json!({"approval_id": approval.approval_id, "decision":"approved"}),
        "other-answer",
    )
    .unwrap_err();
    assert!(error.to_string().contains("another Router"), "{error:#}");

    // A model claim without an approval cannot create authorization.
    let missing = execute(
        &fixture.runtime,
        "CrpAnswer",
        json!({"approval_id": "invented", "decision":"approved"}),
        "invented",
    )
    .unwrap_err();
    assert!(missing.to_string().contains("Approval not found"));

    // Approving twice is rejected; the answer is durable and a replay of the
    // same tool call returns the original result.
    let answered = execute(
        &fixture.runtime,
        "CrpAnswer",
        json!({"approval_id": approval.approval_id, "decision":"approved","note":"ok"}),
        "answer-1",
    )
    .unwrap();
    assert_eq!(answered["status"], "APPROVED");
    let replay = execute(
        &fixture.runtime,
        "CrpAnswer",
        json!({"approval_id": approval.approval_id, "decision":"approved","note":"ok"}),
        "answer-1",
    )
    .unwrap();
    assert_eq!(replay["approval_id"], answered["approval_id"]);
    let again = execute(
        &fixture.runtime,
        "CrpAnswer",
        json!({"approval_id": approval.approval_id, "decision":"rejected"}),
        "answer-2",
    )
    .unwrap_err();
    assert!(again.to_string().contains("already resolved"), "{again:#}");

    // Process the answer so the task returns to implementation, then bump the
    // specification; an approval for the old version can no longer be used.
    fixture
        .store()
        .process_event(answered["event_id"].as_str().unwrap(), &policy)
        .unwrap();
    fixture
        .store()
        .ingest_event(
            &task.task_id,
            NewEvent {
                session_id: None,
                run_id: None,
                event_type: EventType::SpecUpdated,
                causation_id: None,
                operation_id: "bump-spec".into(),
                payload: json!({"spec": fixture.spec()}),
            },
        )
        .unwrap();
    fixture
        .store()
        .process_pending(&fixture.router, &policy)
        .unwrap();
    let pending = fixture
        .store()
        .request_approval(
            "request-old",
            &task.task_id,
            crate::super_agent::store::ApprovalDraft {
                source_session_id: fixture.coder.clone(),
                parent_session_id: fixture.router.clone(),
                request_type: ApprovalType::Plan,
                request: json!({"plan":"old"}),
                task_spec_version: 1,
                permission_policy_version: 1,
            },
        )
        .unwrap();
    let error = execute(
        &fixture.runtime,
        "CrpAnswer",
        json!({"approval_id": pending.approval_id, "decision":"approved"}),
        "stale-answer",
    )
    .unwrap_err();
    assert!(error.to_string().contains("spec version"), "{error:#}");
}

#[test]
fn router_and_member_can_exchange_messages_through_the_session_queue() {
    let fixture = Fixture::new();
    let task = fixture.task();
    let sent = execute(
        &fixture.runtime,
        "CrpSend",
        json!({"task_id": task.task_id, "to":"coder", "content":"Please focus on tests"}),
        "send-1",
    )
    .unwrap();
    assert_eq!(sent["recipient"], fixture.coder);
    assert_eq!(sent["queued"], true);
    let context = ServiceContext::open(
        crate::runtime::RuntimePaths::new(fixture.data.clone()).unwrap(),
        Arc::new(EnvironmentCredentials),
    )
    .unwrap();
    let inbox = crate::services::session_messages::inbox(&context, &fixture.coder, true).unwrap();
    assert_eq!(inbox.len(), 1);
    assert_eq!(inbox[0].content, "Please focus on tests");

    // A crash between delivery and event recording is repeated from the same
    // operation: the member sees one message and the task records one event.
    let delivered = crate::services::session_messages::send_once(
        &context,
        "crp-send:send-retry",
        &fixture.router,
        &fixture.coder,
        "crash retry",
    )
    .unwrap();
    for _ in 0..2 {
        let sent = execute(
            &fixture.runtime,
            "CrpSend",
            json!({"task_id": task.task_id, "to":"coder", "content":"crash retry"}),
            "send-retry",
        )
        .unwrap();
        assert_eq!(sent["message_id"], delivered.id);
    }
    let inbox = crate::services::session_messages::inbox(&context, &fixture.coder, false).unwrap();
    assert_eq!(
        inbox
            .iter()
            .filter(|message| message.content == "crash retry")
            .count(),
        1
    );
    let events = fixture.store().events(&task.task_id).unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event_type == EventType::MessageSent
                && event.payload["message_id"] == delivered.id)
            .count(),
        1
    );

    // An unknown member is rejected instead of guessing a recipient.
    let error = execute(
        &fixture.runtime,
        "CrpSend",
        json!({"task_id": task.task_id, "to":"wrong", "content":"x"}),
        "send-bad",
    )
    .unwrap_err();
    assert!(error.to_string().contains("Unknown member"));
}

#[test]
fn router_cannot_see_edit_tools_and_members_get_only_member_tools() {
    let router = definitions(Role::Router, None);
    let names: Vec<&str> = router
        .iter()
        .map(|definition| definition.name.as_str())
        .collect();
    assert_eq!(names.len(), ROUTER_TOOL_NAMES.len());
    assert!(names.contains(&"CrpCreate"));
    assert!(!names.contains(&"CrpSubmit"));

    let member = definitions(Role::Agent, Some(MemberRole::Coder));
    let names: Vec<&str> = member
        .iter()
        .map(|definition| definition.name.as_str())
        .collect();
    assert_eq!(names, MEMBER_TOOL_NAMES.to_vec());
    let agent = definitions(Role::Agent, None);
    assert!(agent.is_empty());
    assert!(is_forbidden_router_tool("Edit"));
    assert!(is_forbidden_router_tool("NotebookEdit"));
    assert!(!is_forbidden_router_tool("Bash"));
    assert!(!is_forbidden_router_tool("Read"));
}

#[test]
fn propose_task_records_a_pending_proposal_only_for_members() {
    let fixture = Fixture::new();
    let task = fixture.task();
    let value = execute(
        &fixture.member("reviewer"),
        "CrpProposeTask",
        json!({"proposal":{"summary":"Refactor the parser","severity":"low"}}),
        "propose-1",
    )
    .unwrap();
    assert_eq!(value["recorded"], true);
    let events = fixture.store().events(&task.task_id).unwrap();
    assert!(events
        .iter()
        .any(|event| { event.event_type == EventType::NewTaskProposed && !event.processed }));
    let error = execute(
        &fixture.runtime_for(&fixture.outsider),
        "CrpProposeTask",
        json!({"proposal":{"summary":"x"}}),
        "propose-outsider",
    )
    .unwrap_err();
    assert!(error
        .to_string()
        .contains("not a current Coder or Reviewer"));
    assert!(execute(
        &fixture.member("coder"),
        "CrpProposeTask",
        json!({"proposal":{}}),
        "propose-empty"
    )
    .is_err());
}

/// The real CrpSubmit admission for the post-delivery completion review. A
/// FINALIZING task accepts only the current Reviewer's FINAL_DELIVERY review
/// and harness; implementation-stage, stale-round and foreign submissions are
/// rejected by the same tool path production uses.
#[test]
fn finalizing_accepts_only_the_current_final_delivery_review_and_harness() {
    let fixture = Fixture::new();
    let task = fixture.task();
    let store = fixture.store();
    store
        .save_repo(&RepoTarget {
            task_id: task.task_id.clone(),
            repository_id: "repo-code".into(),
            repo_root: "/workspace/code".into(),
            git_common_dir: "/workspace/code/.git".into(),
            target_path: "/workspace/code".into(),
            target_branch: "develop".into(),
            task_branch: "agent/task".into(),
            worktree_path: "/data/worktrees/task".into(),
            working_path: "/data/worktrees/task".into(),
            base_commit: "base".into(),
            submitted_commit: Some("submitted".into()),
            reviewed_commit: Some("reviewed".into()),
            integrated_commit: Some("delivered".into()),
            integration_status: crate::super_agent::IntegrationStatus::Completed,
            verification_status: crate::super_agent::VerificationStatus::Passed,
            source_state: vec![],
        })
        .unwrap();
    let policy = crate::super_agent::state::StatePolicy::default();
    for (event_type, operation, payload) in [
        (EventType::DiscoveryCompleted, "discovery", json!({})),
        (EventType::PlanSubmitted, "plan", json!({})),
        (
            EventType::ApprovalRequested,
            "approval-request",
            json!({"request_type":"PLAN"}),
        ),
        (
            EventType::ApprovalResolved,
            "approval-resolve",
            json!({"request_type":"PLAN","decision":"APPROVED"}),
        ),
        (
            EventType::ImplementationSubmitted,
            "implementation",
            json!({}),
        ),
        (
            EventType::ReviewCompleted,
            "review-1",
            json!({"verdict":"APPROVED"}),
        ),
        (
            EventType::IntegrationPrepared,
            "integration-prepared",
            json!({"repositories":[{"repository_id":"repo-code","tree_changed":true}]}),
        ),
        (
            EventType::ReviewCompleted,
            "review-final",
            json!({"verdict":"APPROVED","stage":"FINAL_DELIVERY"}),
        ),
        (EventType::HarnessAnalysisCompleted, "harness", json!({})),
        (EventType::IntegrationCompleted, "integration", json!({})),
    ] {
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
    }
    assert_eq!(
        store.get_task(&task.task_id).unwrap().unwrap().phase,
        Phase::Finalizing
    );

    let review = |stage: &str, round: u32| {
        json!({
            "kind": "review",
            "payload": {
                "verdict": "APPROVED",
                "spec_version": 1,
                "round": round,
                "stage": stage,
                "reviewed_versions": [{"repository_id":"repo-code","commit":"delivered"}],
                "acceptance_results": [{"criterion":"AC-001","passed":true,"evidence":["git rev-parse"]}],
                "issues": [],
                "verification": ["git rev-parse develop"],
                "limitations": []
            }
        })
    };

    // Only FINAL_DELIVERY is admissible once the task is FINALIZING.
    let rejected = execute(
        &fixture.member("reviewer"),
        "CrpSubmit",
        review("IMPLEMENTATION", 0),
        "finalizing-implementation",
    )
    .unwrap_err();
    assert!(
        rejected
            .to_string()
            .contains("does not match the current review stage FINAL_DELIVERY"),
        "{rejected:#}"
    );
    // A stale round never becomes the completion review.
    let stale = execute(
        &fixture.member("reviewer"),
        "CrpSubmit",
        review("FINAL_DELIVERY", 1),
        "finalizing-stale-round",
    )
    .unwrap_err();
    assert!(stale.to_string().contains("round"), "{stale:#}");
    // Only the task's own Reviewer can submit it.
    let foreign = execute(
        &fixture.runtime_for(&fixture.outsider),
        "CrpSubmit",
        review("FINAL_DELIVERY", 0),
        "finalizing-outsider",
    )
    .unwrap_err();
    assert!(
        foreign
            .to_string()
            .contains("not a current Coder or Reviewer"),
        "{foreign:#}"
    );
    // The harness must also be FINAL_DELIVERY.
    let wrong_harness = execute(
        &fixture.member("reviewer"),
        "CrpSubmit",
        json!({"kind":"harness","payload":{
            "spec_version": 1,
            "round": 0,
            "stage": "IMPLEMENTATION",
            "findings": []
        }}),
        "finalizing-wrong-harness",
    )
    .unwrap_err();
    assert!(
        wrong_harness.to_string().contains("does not match"),
        "{wrong_harness:#}"
    );

    // The authorized pair is recorded through the real tool path.
    let accepted = execute(
        &fixture.member("reviewer"),
        "CrpSubmit",
        review("FINAL_DELIVERY", 0),
        "finalizing-review",
    )
    .unwrap();
    assert!(accepted["report_id"].is_string(), "{accepted}");
    let harness = execute(
        &fixture.member("reviewer"),
        "CrpSubmit",
        json!({"kind":"harness","payload":{
            "spec_version": 1,
            "round": 0,
            "stage": "FINAL_DELIVERY",
            "findings": []
        }}),
        "finalizing-harness",
    )
    .unwrap();
    assert!(harness["report_id"].is_string(), "{harness}");
}
