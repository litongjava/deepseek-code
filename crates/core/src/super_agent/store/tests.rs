use super::*;
use crate::super_agent::model::*;
use crate::super_agent::state::StatePolicy;
use serde_json::{json, Value};
use std::sync::Arc;

fn fixture() -> (tempfile::TempDir, std::path::PathBuf, TaskStore) {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let paths = crate::runtime::RuntimePaths::new(data.clone()).unwrap();
    let store = TaskStore::new(paths.open_database().unwrap(), data.clone());
    (temp, data, store)
}

fn spec(title: &str) -> TaskSpec {
    TaskSpec {
        title: title.into(),
        workspace: std::path::PathBuf::from("/workspace"),
        targets: vec!["/workspace/code".into()],
        goal: "Complete the task".into(),
        requirements: vec!["Keep behavior".into()],
        acceptance_criteria: vec![AcceptanceCriterion {
            id: "AC-001".into(),
            description: "Tests pass".into(),
        }],
        constraints: vec![],
        out_of_scope: vec![],
        target_branch: Some("develop".into()),
        repository_target_branches: Default::default(),
    }
}

fn event(event_type: EventType, operation_id: &str, payload: Value) -> NewEvent {
    NewEvent {
        session_id: None,
        run_id: None,
        event_type,
        causation_id: None,
        operation_id: operation_id.into(),
        payload,
    }
}

fn repo(task_id: &str) -> RepoTarget {
    RepoTarget {
        task_id: task_id.into(),
        repository_id: "repo-code".into(),
        repo_root: "/workspace/code".into(),
        git_common_dir: "/workspace/code/.git".into(),
        target_path: "/workspace/code".into(),
        target_branch: "develop".into(),
        task_branch: format!("agent/{task_id}"),
        worktree_path: "/data/worktrees/task".into(),
        working_path: "/data/worktrees/task".into(),
        base_commit: "base".into(),
        submitted_commit: Some("submitted".into()),
        reviewed_commit: Some("integrated".into()),
        integrated_commit: Some("integrated".into()),
        integration_status: IntegrationStatus::Completed,
        verification_status: VerificationStatus::Passed,
        source_state: vec![],
    }
}

fn finalizing(store: &TaskStore) -> TaskRecord {
    let policy = StatePolicy::default();
    let task = store
        .create_task("router", spec("Fix login"), "create")
        .unwrap();
    store.save_repo(&repo(&task.task_id)).unwrap();
    store
        .set_sessions(
            &task.task_id,
            Some("coder-session"),
            Some("reviewer-session"),
        )
        .unwrap();
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
        (EventType::IntegrationCompleted, "integration", json!({})),
        (
            EventType::ReviewCompleted,
            "review-final",
            json!({"verdict":"APPROVED"}),
        ),
    ] {
        store
            .apply_event(
                &task.task_id,
                event(event_type, operation, payload),
                &policy,
            )
            .unwrap();
    }
    store
        .add_report(
            "report-review",
            &task.task_id,
            ReportKind::Review,
            1,
            0,
            json!({
                "verdict":"APPROVED",
                "spec_version":1,
                "stage":"FINAL_DELIVERY",
                "reviewed_versions":[{"repository_id":"repo-code","commit":"integrated"}],
                "verification":["cargo test"],
                "acceptance_results":[{"criterion":"AC-001","passed":true,"evidence":["cargo test"]}]
            }),
        )
        .unwrap();
    store
        .add_report(
            "report-harness",
            &task.task_id,
            ReportKind::Harness,
            1,
            0,
            json!({"spec_version":1,"findings":[]}),
        )
        .unwrap();
    store
        .add_report(
            "report-implementation",
            &task.task_id,
            ReportKind::Implementation,
            1,
            0,
            json!({"spec_version":1,"summary":"implemented","tests":[{"command":"cargo test","passed":true}]}),
        )
        .unwrap();
    let record = store.get_task(&task.task_id).unwrap().unwrap();
    assert_eq!(record.phase, Phase::Finalizing);
    record
}

#[test]
fn task_creation_is_globally_idempotent_and_ids_are_sequential() {
    let (_temp, _data, store) = fixture();
    let first = store
        .create_task("router", spec("First"), "create-first")
        .unwrap();
    assert_eq!(first.task_id, "task-001");
    assert_eq!(first.pair_id, "crp-001");
    assert_eq!(first.phase, Phase::Discovering);
    assert_eq!(first.status, Status::Queued);
    assert_eq!(first.spec_version, 1);
    assert_eq!(store.events(&first.task_id).unwrap().len(), 1);
    let replay = store
        .create_task("router", spec("First"), "create-first")
        .unwrap();
    assert_eq!(replay.task_id, first.task_id);
    assert_eq!(store.list_tasks("router", None).unwrap().len(), 1);
    let conflict = store
        .create_task("router", spec("Different"), "create-first")
        .unwrap_err();
    assert!(matches!(conflict, StoreError::OperationConflict { .. }));
    let second = store
        .create_task("router", spec("Second"), "create-second")
        .unwrap();
    assert_eq!(second.task_id, "task-002");
    assert_eq!(second.pair_id, "crp-002");
}

#[test]
fn transitions_persist_and_invalid_events_are_rejected_without_writing() {
    let (_temp, data, store) = fixture();
    let policy = StatePolicy::default();
    let task = store
        .create_task("router", spec("Transition"), "create")
        .unwrap();
    let error = store
        .apply_event(
            &task.task_id,
            event(EventType::ImplementationSubmitted, "bad", json!({})),
            &policy,
        )
        .unwrap_err();
    assert!(matches!(error, StoreError::Transition(_)));
    assert_eq!(
        store.get_task(&task.task_id).unwrap().unwrap().phase,
        Phase::Discovering
    );
    assert_eq!(store.events(&task.task_id).unwrap().len(), 1);

    let applied = store
        .apply_event(
            &task.task_id,
            event(EventType::DiscoveryCompleted, "discovery", json!({})),
            &policy,
        )
        .unwrap();
    assert!(applied.inserted);
    assert_eq!(applied.event.sequence, 2);
    let replay = store
        .apply_event(
            &task.task_id,
            event(EventType::DiscoveryCompleted, "discovery", json!({})),
            &policy,
        )
        .unwrap();
    assert!(!replay.inserted);
    assert_eq!(replay.event.event_id, applied.event.event_id);
    assert_eq!(store.events(&task.task_id).unwrap().len(), 2);
    let conflict = store
        .apply_event(
            &task.task_id,
            event(
                EventType::DiscoveryCompleted,
                "discovery",
                json!({"changed":true}),
            ),
            &policy,
        )
        .unwrap_err();
    assert!(matches!(conflict, StoreError::OperationConflict { .. }));

    let record = store.get_task(&task.task_id).unwrap().unwrap();
    assert_eq!(record.phase, Phase::Planning);
    assert_eq!(record.status, Status::Running);

    // Reopening the database sees the committed transition and event.
    let reopened = crate::db::DbManager::new(data.join("deepseek_code.db")).unwrap();
    reopened.init().unwrap();
    let reopened = TaskStore::new(Arc::new(reopened), data.clone());
    let persisted = reopened.get_task(&task.task_id).unwrap().unwrap();
    assert_eq!(persisted.phase, Phase::Planning);
    assert_eq!(reopened.events(&task.task_id).unwrap().len(), 2);
}

#[test]
fn spec_updates_invalidate_old_reports_and_expire_old_approvals() {
    let (_temp, _data, store) = fixture();
    let policy = StatePolicy::default();
    let task = store.create_task("router", spec("Spec"), "create").unwrap();
    store
        .add_report(
            "old-report",
            &task.task_id,
            ReportKind::Review,
            1,
            0,
            json!({"verdict":"APPROVED","spec_version":1,"stage":"FINAL_DELIVERY"}),
        )
        .unwrap();
    let approval = store
        .request_approval(
            "old-approval",
            &task.task_id,
            ApprovalDraft {
                source_session_id: "coder".into(),
                parent_session_id: "router".into(),
                request_type: ApprovalType::Plan,
                request: json!({}),
                task_spec_version: 1,
                permission_policy_version: 1,
            },
        )
        .unwrap();
    store
        .apply_event(
            &task.task_id,
            event(EventType::DiscoveryCompleted, "discovery", json!({})),
            &policy,
        )
        .unwrap();
    let mut next = spec("Spec changed");
    next.goal = "New goal".into();
    let applied = store
        .apply_event(
            &task.task_id,
            event(EventType::SpecUpdated, "spec-update", json!({"spec":next})),
            &policy,
        )
        .unwrap();
    assert!(applied.inserted);
    let record = store.get_task(&task.task_id).unwrap().unwrap();
    assert_eq!(record.spec_version, 2);
    assert_eq!(record.phase, Phase::Planning);
    let reports = store.reports(&task.task_id).unwrap();
    assert!(reports.iter().all(|report| report.invalidated));
    let approvals = store.approvals(&task.task_id).unwrap();
    assert_eq!(approvals[0].approval_id, approval.approval_id);
    assert_eq!(approvals[0].status, ApprovalStatus::Expired);
}

#[test]
fn completion_requires_the_gate_and_records_the_event() {
    let (_temp, data, store) = fixture();
    let policy = StatePolicy::default();
    let record = finalizing(&store);
    let completed = store
        .complete_task(&record.task_id, "complete", &policy)
        .unwrap();
    assert_eq!(completed.status, Status::Completed);
    assert_eq!(completed.phase, Phase::Finalizing);
    let events = store.events(&record.task_id).unwrap();
    assert_eq!(events.last().unwrap().event_type, EventType::TaskCompleted);
    let reopened = crate::db::DbManager::new(data.join("deepseek_code.db")).unwrap();
    reopened.init().unwrap();
    let reopened = TaskStore::new(Arc::new(reopened), data.clone());
    assert_eq!(
        reopened.get_task(&record.task_id).unwrap().unwrap().status,
        Status::Completed
    );
    // Replaying the completion is idempotent.
    assert!(
        !reopened
            .apply_event(
                &record.task_id,
                event(EventType::TaskCompleted, "complete", json!({})),
                &policy,
            )
            .unwrap()
            .inserted
    );
}

#[test]
fn an_unmet_gate_leaves_the_task_untouched() {
    let (_temp, _data, store) = fixture();
    let policy = StatePolicy::default();
    let task = store.create_task("router", spec("Gate"), "create").unwrap();
    // Advance to FINALIZING without reports or sessions.
    store.save_repo(&repo(&task.task_id)).unwrap();
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
            "review",
            json!({"verdict":"APPROVED"}),
        ),
        (EventType::IntegrationCompleted, "integration", json!({})),
        (
            EventType::ReviewCompleted,
            "review-final",
            json!({"verdict":"APPROVED"}),
        ),
    ] {
        store
            .apply_event(
                &task.task_id,
                event(event_type, operation, payload),
                &policy,
            )
            .unwrap();
    }
    let error = store
        .complete_task(&task.task_id, "complete", &policy)
        .unwrap_err();
    match error {
        StoreError::Gate(unmet) => {
            assert!(unmet
                .iter()
                .any(|item| matches!(item, Unmet::FinalReviewMissing)));
            assert!(unmet
                .iter()
                .any(|item| matches!(item, Unmet::SessionMissing { .. })));
        }
        other => panic!("unexpected error: {other:?}"),
    }
    let record = store.get_task(&task.task_id).unwrap().unwrap();
    assert_eq!(record.status, Status::Running);
    assert_eq!(record.phase, Phase::Finalizing);
    assert!(store
        .events(&task.task_id)
        .unwrap()
        .iter()
        .all(|event| event.event_type != EventType::TaskCompleted));
}

#[test]
fn reports_approvals_and_locks_are_idempotent_and_reject_conflicting_reuse() {
    let (_temp, _data, store) = fixture();
    let task = store
        .create_task("router", spec("Idempotent"), "create")
        .unwrap();
    let report = store
        .add_report(
            "report-op",
            &task.task_id,
            ReportKind::Plan,
            1,
            0,
            json!({"spec_version":1,"understanding":"done"}),
        )
        .unwrap();
    let replay = store
        .add_report(
            "report-op",
            &task.task_id,
            ReportKind::Plan,
            1,
            0,
            json!({"spec_version":1,"understanding":"done"}),
        )
        .unwrap();
    assert_eq!(report.report_id, replay.report_id);
    assert!(matches!(
        store
            .add_report(
                "report-op",
                &task.task_id,
                ReportKind::Plan,
                1,
                0,
                json!({"spec_version":1,"understanding":"different"}),
            )
            .unwrap_err(),
        StoreError::OperationConflict { .. }
    ));

    let approval = store
        .request_approval(
            "approval-op",
            &task.task_id,
            ApprovalDraft {
                source_session_id: "coder".into(),
                parent_session_id: "router".into(),
                request_type: ApprovalType::Plan,
                request: json!({"plan":"x"}),
                task_spec_version: 1,
                permission_policy_version: 1,
            },
        )
        .unwrap();
    let replay = store
        .request_approval(
            "approval-op",
            &task.task_id,
            ApprovalDraft {
                source_session_id: "coder".into(),
                parent_session_id: "router".into(),
                request_type: ApprovalType::Plan,
                request: json!({"plan":"x"}),
                task_spec_version: 1,
                permission_policy_version: 1,
            },
        )
        .unwrap();
    assert_eq!(approval.approval_id, replay.approval_id);
    let resolved = store
        .resolve_approval(
            "resolve-op",
            &approval.approval_id,
            ApprovalStatus::Approved,
            Some("go"),
        )
        .unwrap();
    assert_eq!(resolved.status, ApprovalStatus::Approved);
    assert!(matches!(
        store
            .resolve_approval(
                "resolve-op-2",
                &approval.approval_id,
                ApprovalStatus::Rejected,
                None,
            )
            .unwrap_err(),
        StoreError::AlreadyResolved(_)
    ));

    assert_eq!(
        store
            .try_lock("lock-op", "branch:/repo/.git:develop", &task.task_id)
            .unwrap(),
        LockOutcome::Acquired
    );
    assert_eq!(
        store
            .try_lock("lock-op", "branch:/repo/.git:develop", &task.task_id)
            .unwrap(),
        LockOutcome::AlreadyOwned
    );
    assert!(matches!(
        store
            .try_lock("different-op", "branch:/repo/.git:develop", &task.task_id)
            .unwrap_err(),
        StoreError::LockBusy(_)
    ));
    assert!(store
        .unlock("branch:/repo/.git:develop", &task.task_id, "lock-op")
        .unwrap());
    assert!(!store
        .unlock("branch:/repo/.git:develop", &task.task_id, "lock-op")
        .unwrap());
}

#[test]
fn artifacts_are_written_atomically_and_names_cannot_escape_the_task_directory() {
    let (temp, data, store) = fixture();
    let task = store
        .create_task("router", spec("Artifact"), "create")
        .unwrap();
    let path = store
        .write_artifact(&task.task_id, "tests.log", b"passed\n")
        .unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "passed\n");
    assert!(path.starts_with(data.join("super-agent").join(&task.task_id)));
    assert!(store
        .write_artifact(&task.task_id, "../escape", b"x")
        .is_err());
    assert!(store
        .write_artifact(&task.task_id, ".hidden", b"x")
        .is_err());
    assert!(store.write_artifact("../task", "name", b"x").is_err());
    assert!(temp.path().exists());
}

#[test]
fn concurrent_same_operation_creates_one_event_and_one_task() {
    let (_temp, _data, store) = fixture();
    let policy = StatePolicy::default();
    let task = store
        .create_task("router", spec("Concurrent"), "create")
        .unwrap();
    let mut handles = Vec::new();
    for _ in 0..8 {
        let store = store.clone();
        let task_id = task.task_id.clone();
        handles.push(std::thread::spawn(move || {
            store.apply_event(
                &task_id,
                event(
                    EventType::NewTaskProposed,
                    "same-op",
                    json!({"proposal":"x"}),
                ),
                &policy,
            )
        }));
    }
    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    let event_ids: std::collections::BTreeSet<_> = results
        .iter()
        .map(|result| result.as_ref().unwrap().event.event_id.clone())
        .collect();
    assert_eq!(event_ids.len(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| result.as_ref().unwrap().inserted)
            .count(),
        1
    );
    assert_eq!(store.events(&task.task_id).unwrap().len(), 2);

    let mut handles = Vec::new();
    for index in 0..8 {
        let store = store.clone();
        handles.push(std::thread::spawn(move || {
            store.create_task("router", spec("Concurrent"), "same-create")
        }));
    }
    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    assert!(results.iter().all(|result| result.is_ok()));
    let ids: std::collections::BTreeSet<_> = results
        .iter()
        .map(|result| result.as_ref().unwrap().task_id.clone())
        .collect();
    assert_eq!(ids.len(), 1);
    assert_eq!(store.list_tasks("router", None).unwrap().len(), 2);
}

#[test]
fn concurrent_conflicting_operations_accept_exactly_one_payload() {
    let (_temp, _data, store) = fixture();
    let policy = StatePolicy::default();
    let task = store
        .create_task("router", spec("Conflict"), "create")
        .unwrap();
    let mut handles = Vec::new();
    for index in 0..8 {
        let store = store.clone();
        let task_id = task.task_id.clone();
        handles.push(std::thread::spawn(move || {
            store.apply_event(
                &task_id,
                event(
                    EventType::NewTaskProposed,
                    "conflict-op",
                    json!({"proposal":index}),
                ),
                &policy,
            )
        }));
    }
    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| { matches!(result, Err(StoreError::OperationConflict { .. })) })
            .count(),
        7
    );
    assert_eq!(store.events(&task.task_id).unwrap().len(), 2);
}

#[test]
fn concurrent_distinct_events_get_unique_contiguous_sequences() {
    let (_temp, _data, store) = fixture();
    let policy = StatePolicy::default();
    let task = store
        .create_task("router", spec("Sequences"), "create")
        .unwrap();
    let mut handles = Vec::new();
    for index in 0..8 {
        let store = store.clone();
        let task_id = task.task_id.clone();
        handles.push(std::thread::spawn(move || {
            store.apply_event(
                &task_id,
                event(
                    EventType::NewTaskProposed,
                    &format!("sequence-{index}"),
                    json!({"index":index}),
                ),
                &policy,
            )
        }));
    }
    for handle in handles {
        handle.join().unwrap().unwrap();
    }
    let sequences: Vec<u64> = store
        .events(&task.task_id)
        .unwrap()
        .into_iter()
        .map(|event| event.sequence)
        .collect();
    assert_eq!(sequences, (1..=9).collect::<Vec<_>>());
}

#[test]
fn only_one_concurrent_task_can_hold_a_lock() {
    let (_temp, _data, store) = fixture();
    let mut tasks = Vec::new();
    for index in 0..8 {
        tasks.push(
            store
                .create_task("router", spec("Locks"), &format!("create-{index}"))
                .unwrap(),
        );
    }
    let mut handles = Vec::new();
    for task in tasks {
        let store = store.clone();
        handles.push(std::thread::spawn(move || {
            let operation = format!("shared-lock-{}", task.task_id);
            store
                .try_lock(&operation, "path:/workspace/scripts", &task.task_id)
                .map(|_| task.task_id)
        }));
    }
    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(StoreError::LockBusy(_))))
            .count(),
        7
    );
}

#[test]
fn cleanup_status_and_coder_replacement_are_persisted() {
    let (_temp, _data, store) = fixture();
    let task = store
        .create_task("router", spec("Cleanup"), "create")
        .unwrap();
    store
        .set_sessions(&task.task_id, Some("coder-1"), Some("reviewer-1"))
        .unwrap();
    store
        .replace_coder_session(&task.task_id, "coder-2")
        .unwrap();
    let record = store.get_task(&task.task_id).unwrap().unwrap();
    assert_eq!(record.coder.session_id.as_deref(), Some("coder-2"));
    assert_eq!(record.coder.previous_session_ids, vec!["coder-1"]);
    assert_eq!(record.reviewer.session_id.as_deref(), Some("reviewer-1"));
    store
        .set_cleanup_status(&task.task_id, CleanupStatus::Pending)
        .unwrap();
    assert_eq!(
        store
            .get_task(&task.task_id)
            .unwrap()
            .unwrap()
            .cleanup_status,
        CleanupStatus::Pending
    );
}
