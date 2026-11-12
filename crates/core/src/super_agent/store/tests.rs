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
    // Sessions must exist in the durable store; a nonempty string is not proof.
    let sessions = crate::runtime::SessionStore::new(store.db().clone());
    let coder = sessions
        .create(store.data_dir(), "fixture", "fixture")
        .unwrap();
    let reviewer = sessions
        .create(store.data_dir(), "fixture", "fixture")
        .unwrap();
    // A durable member must also have a real transcript.
    sessions
        .append(&coder, "user", "implement the task")
        .unwrap();
    sessions
        .append(&reviewer, "user", "review the task")
        .unwrap();
    store
        .set_sessions(&task.task_id, Some(&coder), Some(&reviewer))
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

#[test]
fn report_validation_binds_version_round_and_typed_body() {
    let (_temp, _data, store) = fixture();
    let task = store
        .create_task("router", spec("Reports"), "create")
        .unwrap();
    let review = json!({"verdict":"APPROVED","spec_version":1,"stage":"FINAL_DELIVERY"});
    assert!(matches!(
        store
            .add_report(
                "wrong-version",
                &task.task_id,
                ReportKind::Review,
                2,
                0,
                review.clone()
            )
            .unwrap_err(),
        StoreError::Invalid(_)
    ));
    assert!(matches!(
        store
            .add_report(
                "wrong-round",
                &task.task_id,
                ReportKind::Review,
                1,
                1,
                review.clone()
            )
            .unwrap_err(),
        StoreError::Invalid(_)
    ));
    let mut mismatched = review.clone();
    mismatched["spec_version"] = json!(2);
    assert!(matches!(
        store
            .add_report(
                "mismatched-body",
                &task.task_id,
                ReportKind::Review,
                1,
                0,
                mismatched
            )
            .unwrap_err(),
        StoreError::Invalid(_)
    ));
    assert!(matches!(
        store
            .add_report(
                "missing-field",
                &task.task_id,
                ReportKind::Implementation,
                1,
                0,
                json!({"spec_version":1})
            )
            .unwrap_err(),
        StoreError::Invalid(_)
    ));
    let report = store
        .add_report("valid", &task.task_id, ReportKind::Review, 1, 0, review)
        .unwrap();
    assert_eq!((report.spec_version, report.round), (1, 0));
}

fn gate_unmet(result: Result<TaskRecord, StoreError>) -> Vec<Unmet> {
    match result {
        Err(StoreError::Gate(unmet)) => unmet,
        Ok(record) => panic!("task unexpectedly completed: {record:?}"),
        Err(error) => panic!("unexpected store error: {error:?}"),
    }
}

#[test]
fn completion_requires_durable_sessions_and_trace() {
    let (_temp, _data, store) = fixture();
    let policy = StatePolicy::default();
    let record = finalizing(&store);
    let coder = record.coder.session_id.clone().unwrap();
    let reviewer = record.reviewer.session_id.clone().unwrap();

    store
        .set_sessions(
            &record.task_id,
            Some("missing-coder"),
            Some("missing-reviewer"),
        )
        .unwrap();
    let unmet = gate_unmet(store.complete_task(&record.task_id, "complete-1", &policy));
    assert_eq!(
        unmet
            .iter()
            .filter(|item| matches!(item, Unmet::SessionUnverified { .. }))
            .count(),
        2,
        "{unmet:?}"
    );
    assert_eq!(
        store.get_task(&record.task_id).unwrap().unwrap().status,
        Status::Running
    );

    store
        .set_sessions(&record.task_id, Some(&coder), Some(&reviewer))
        .unwrap();
    store
        .db()
        .with_conn(|conn| {
            conn.execute(
                "DELETE FROM sa_events WHERE task_id=?1 AND event_type=?2",
                rusqlite::params![record.task_id, "HARNESS_ANALYSIS_COMPLETED"],
            )
        })
        .unwrap()
        .unwrap();
    let unmet = gate_unmet(store.complete_task(&record.task_id, "complete-2", &policy));
    assert!(
        unmet.iter().any(|item| matches!(
            item,
            Unmet::TraceMissing {
                event: EventType::HarnessAnalysisCompleted
            }
        )),
        "{unmet:?}"
    );
}

/// A FINALIZING task whose implementation review covers `reviewed-r` while the
/// host-verified integration produced a different merge commit `delivered-m`.
fn finalizing_with_a_merge_commit(store: &TaskStore) -> TaskRecord {
    let policy = StatePolicy::default();
    let task = store
        .create_task("router", spec("Merge delivery"), "create-merge")
        .unwrap();
    let mut merge_repo = repo(&task.task_id);
    merge_repo.reviewed_commit = Some("reviewed-r".into());
    merge_repo.integrated_commit = Some("delivered-m".into());
    store.save_repo(&merge_repo).unwrap();
    let sessions = crate::runtime::SessionStore::new(store.db().clone());
    let coder = sessions
        .create(store.data_dir(), "fixture", "fixture")
        .unwrap();
    let reviewer = sessions
        .create(store.data_dir(), "fixture", "fixture")
        .unwrap();
    sessions
        .append(&coder, "user", "implement the task")
        .unwrap();
    sessions
        .append(&reviewer, "user", "review the task")
        .unwrap();
    store
        .set_sessions(&task.task_id, Some(&coder), Some(&reviewer))
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
                event(event_type, operation, payload),
                &policy,
            )
            .unwrap();
    }
    store
        .add_report(
            "merge-review",
            &task.task_id,
            ReportKind::Review,
            1,
            0,
            json!({
                "verdict":"APPROVED",
                "spec_version":1,
                "round":0,
                "stage":"FINAL_DELIVERY",
                "reviewed_versions":[{"repository_id":"repo-code","commit":"delivered-m"}],
                "verification":["git rev-parse develop"],
                "acceptance_results":[{"criterion":"AC-001","passed":true,"evidence":["git rev-parse develop"]}]
            }),
        )
        .unwrap();
    store
        .add_report(
            "merge-harness",
            &task.task_id,
            ReportKind::Harness,
            1,
            0,
            json!({"spec_version":1,"round":0,"stage":"FINAL_DELIVERY","findings":[]}),
        )
        .unwrap();
    store
        .add_report(
            "merge-implementation",
            &task.task_id,
            ReportKind::Implementation,
            1,
            0,
            json!({"spec_version":1,"round":0,"summary":"implemented","commits":["HEAD"]}),
        )
        .unwrap();
    store.get_task(&task.task_id).unwrap().unwrap()
}

fn merge_plan(
    store: &TaskStore,
    record: &TaskRecord,
    round: u32,
) -> crate::super_agent::delivery::DeliveryPlan {
    use crate::super_agent::delivery::{DeliveryDestination, DeliveryPlan, DeliveryTargetPlan};
    let events = store.events(&record.task_id).unwrap();
    let prepared = events
        .iter()
        .find(|event| event.event_type == EventType::IntegrationPrepared)
        .expect("preparation event");
    let decision = events
        .iter()
        .rev()
        .find(|event| {
            event.event_type == EventType::ReviewCompleted
                && event.payload["stage"].as_str() == Some("FINAL_DELIVERY")
        })
        .expect("final delivery decision");
    DeliveryPlan {
        operation_id: "plan-merge".into(),
        task_id: record.task_id.clone(),
        spec_version: 1,
        review_round: round,
        evidence_event_id: prepared.event_id.clone(),
        review_event_id: decision.event_id.clone(),
        content_fingerprint: "fingerprint".into(),
        created_at: "2026-01-01T00:00:00Z".into(),
        repositories: vec![DeliveryTargetPlan {
            repository_id: "repo-code".into(),
            repo_root: "/workspace/code".into(),
            git_common_dir: "/workspace/code/.git".into(),
            target_branch: "develop".into(),
            target_ref: "refs/heads/develop".into(),
            destination: DeliveryDestination::RefOnly,
            baseline_commit: "base".into(),
            reviewed_commit: "reviewed-r".into(),
            integrated_commit: "delivered-m".into(),
            integrated_tree: "tree-m".into(),
            old_oid: "base".into(),
            new_oid: "delivered-m".into(),
            review_stage: crate::super_agent::ReviewStage::FinalDelivery,
            lock_key: "branch:/workspace/code/.git:refs/heads/develop".into(),
            lock_operation_id: "lock-op".into(),
            attempt: 1,
        }],
    }
}

fn record_plan(store: &TaskStore, plan: &crate::super_agent::delivery::DeliveryPlan) {
    let encoded = serde_json::to_string(plan).unwrap();
    store
        .db()
        .with_conn(|conn| {
            conn.execute(
                "INSERT INTO sa_delivery_plans(operation_id,task_id,spec_version,review_round,created_at,plan_json) VALUES(?1,?2,?3,?4,?5,?6)",
                rusqlite::params![
                    plan.operation_id,
                    plan.task_id,
                    plan.spec_version,
                    plan.review_round,
                    plan.created_at,
                    encoded
                ],
            )
        })
        .unwrap()
        .unwrap();
}

fn record_observation(
    store: &TaskStore,
    plan: &crate::super_agent::delivery::DeliveryPlan,
    coder: &str,
) {
    for target in &plan.repositories {
        store
            .db()
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO sa_repository_delivery_observations(task_id,plan_operation_id,repository_id,spec_version,review_round,coder_session_id,old_oid,new_oid,target_ref,attempt,observed_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
                    rusqlite::params![
                        plan.task_id,
                        plan.operation_id,
                        target.repository_id,
                        plan.spec_version,
                        plan.review_round,
                        coder,
                        target.old_oid,
                        target.new_oid,
                        target.target_ref,
                        target.attempt,
                        "2026-01-01T00:00:00Z"
                    ],
                )
            })
            .unwrap()
            .unwrap();
    }
}

/// The store gate itself, not the orchestrator, must prove the persisted
/// authoritative plan and its complete observations before accepting a final
/// review of a delivered merge commit that differs from the implementation
/// review commit.
#[test]
fn completion_with_a_merge_commit_requires_the_persisted_delivery_plan() {
    let (_temp, _data, store) = fixture();
    let policy = StatePolicy::default();
    let record = finalizing_with_a_merge_commit(&store);
    let coder = record.coder.session_id.clone().unwrap();
    let plan = merge_plan(&store, &record, 0);

    // No plan at all: the differing OIDs cannot be justified.
    let unmet = gate_unmet(store.complete_task(&record.task_id, "complete-1", &policy));
    assert!(
        unmet.iter().any(|item| matches!(
            item,
            Unmet::VersionMismatch { repository_id, .. } if repository_id == "repo-code"
        )),
        "{unmet:?}"
    );

    // A plan without every planned observation is not enough either.
    record_plan(&store, &plan);
    let unmet = gate_unmet(store.complete_task(&record.task_id, "complete-2", &policy));
    assert!(
        unmet.iter().any(|item| matches!(
            item,
            Unmet::VersionMismatch { repository_id, .. } if repository_id == "repo-code"
        )),
        "{unmet:?}"
    );

    // The persisted plan plus the complete observation accepts the exact
    // reviewed lineage and delivered OID.
    record_observation(&store, &plan, &coder);
    let completed = store
        .complete_task(&record.task_id, "complete-3", &policy)
        .unwrap();
    assert_eq!(completed.status, Status::Completed);
    assert_eq!(completed.phase, Phase::Finalizing);
    // The implementation-review commit is preserved, never rewritten to the
    // delivered OID.
    assert_eq!(
        store
            .get_task(&record.task_id)
            .unwrap()
            .unwrap()
            .repositories[0]
            .reviewed_commit
            .as_deref(),
        Some("reviewed-r")
    );
}

#[test]
fn completion_rejects_a_tampered_or_stale_reviewed_lineage() {
    // A tampered reviewed commit cannot ride on a report naming the delivered
    // OID, even with a genuine plan and observation.
    {
        let (_temp, _data, store) = fixture();
        let policy = StatePolicy::default();
        let record = finalizing_with_a_merge_commit(&store);
        let coder = record.coder.session_id.clone().unwrap();
        let plan = merge_plan(&store, &record, 0);
        record_plan(&store, &plan);
        record_observation(&store, &plan, &coder);
        let mut tampered = record.repositories[0].clone();
        tampered.reviewed_commit = Some("unrelated".into());
        store.save_repo(&tampered).unwrap();
        let unmet = gate_unmet(store.complete_task(&record.task_id, "complete", &policy));
        assert!(
            unmet.iter().any(|item| matches!(
                item,
                Unmet::VersionMismatch { repository_id, .. } if repository_id == "repo-code"
            )),
            "{unmet:?}"
        );
        assert_ne!(
            store.get_task(&record.task_id).unwrap().unwrap().status,
            Status::Completed
        );
        assert_eq!(
            store
                .get_task(&record.task_id)
                .unwrap()
                .unwrap()
                .repositories[0]
                .reviewed_commit
                .as_deref(),
            Some("unrelated"),
            "the rejected completion must not rewrite the record it rejected"
        );
    }

    // A plan for another round is stale and cannot justify this version.
    {
        let (_temp, _data, store) = fixture();
        let policy = StatePolicy::default();
        let record = finalizing_with_a_merge_commit(&store);
        let coder = record.coder.session_id.clone().unwrap();
        let stale = merge_plan(&store, &record, 1);
        record_plan(&store, &stale);
        record_observation(&store, &stale, &coder);
        let unmet = gate_unmet(store.complete_task(&record.task_id, "complete", &policy));
        assert!(
            unmet.iter().any(|item| matches!(
                item,
                Unmet::VersionMismatch { repository_id, .. } if repository_id == "repo-code"
            )),
            "{unmet:?}"
        );
        assert_ne!(
            store.get_task(&record.task_id).unwrap().unwrap().status,
            Status::Completed
        );
    }
}

#[test]
fn completion_rejects_a_malformed_current_delivery_plan() {
    let (_temp, _data, store) = fixture();
    let policy = StatePolicy::default();
    let record = finalizing_with_a_merge_commit(&store);
    store
        .db()
        .with_conn(|conn| {
            conn.execute(
                "INSERT INTO sa_delivery_plans(operation_id,task_id,spec_version,review_round,created_at,plan_json) VALUES(?1,?2,?3,?4,?5,?6)",
                rusqlite::params![
                    "plan-broken",
                    record.task_id,
                    1i64,
                    0i64,
                    "2026-01-02T00:00:00Z",
                    "{not json"
                ],
            )
        })
        .unwrap()
        .unwrap();
    let error = store
        .complete_task(&record.task_id, "complete-malformed", &policy)
        .unwrap_err();
    assert!(
        matches!(&error, StoreError::Invalid(message) if message.contains("unreadable")),
        "{error:?}"
    );
    assert_ne!(
        store.get_task(&record.task_id).unwrap().unwrap().status,
        Status::Completed
    );
    // The contradictory record is preserved for inspection, never rewritten
    // into a completion or silently replaced by an older plan.
    let count: i64 = store
        .db()
        .with_conn(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM sa_delivery_plans WHERE task_id=?1",
                [&record.task_id],
                |row| row.get(0),
            )
        })
        .unwrap()
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn completion_rejects_a_plan_without_its_current_approval_evidence() {
    let (_temp, _data, store) = fixture();
    let policy = StatePolicy::default();
    let record = finalizing_with_a_merge_commit(&store);
    let coder = record.coder.session_id.clone().unwrap();

    // The plan names an approval event that does not exist.
    let mut stale = merge_plan(&store, &record, 0);
    stale.review_event_id = "missing-decision".into();
    record_plan(&store, &stale);
    record_observation(&store, &stale, &coder);
    let unmet = gate_unmet(store.complete_task(&record.task_id, "complete-1", &policy));
    assert!(
        unmet.iter().any(|item| matches!(
            item,
            Unmet::VersionMismatch { repository_id, .. } if repository_id == "repo-code"
        )),
        "{unmet:?}"
    );

    // A plan whose preparation event is missing is equally unusable.
    let mut unbound = merge_plan(&store, &record, 0);
    unbound.operation_id = "plan-unbound".into();
    unbound.evidence_event_id = "missing-preparation".into();
    record_plan(&store, &unbound);
    let unmet = gate_unmet(store.complete_task(&record.task_id, "complete-2", &policy));
    assert!(
        unmet.iter().any(|item| matches!(
            item,
            Unmet::VersionMismatch { repository_id, .. } if repository_id == "repo-code"
        )),
        "{unmet:?}"
    );
    assert_ne!(
        store.get_task(&record.task_id).unwrap().unwrap().status,
        Status::Completed
    );
}

#[test]
fn completion_rejects_narrowed_or_duplicate_plan_membership() {
    let (_temp, _data, store) = fixture();
    let policy = StatePolicy::default();
    let record = finalizing_with_a_merge_commit(&store);
    let coder = record.coder.session_id.clone().unwrap();

    // A plan that lists the same repository twice is not the task's set.
    let mut duplicated = merge_plan(&store, &record, 0);
    let target = duplicated.repositories[0].clone();
    duplicated.repositories.push(target);
    record_plan(&store, &duplicated);
    let unmet = gate_unmet(store.complete_task(&record.task_id, "complete-dup", &policy));
    assert!(
        unmet.iter().any(|item| matches!(
            item,
            Unmet::VersionMismatch { repository_id, .. } if repository_id == "repo-code"
        )),
        "{unmet:?}"
    );

    // A plan that narrows the task to no repository is equally unusable.
    let mut narrowed = merge_plan(&store, &record, 0);
    narrowed.operation_id = "plan-narrowed".into();
    narrowed.repositories.clear();
    record_plan(&store, &narrowed);
    let unmet = gate_unmet(store.complete_task(&record.task_id, "complete-empty", &policy));
    assert!(
        unmet.iter().any(|item| matches!(
            item,
            Unmet::VersionMismatch { repository_id, .. } if repository_id == "repo-code"
        )),
        "{unmet:?}"
    );
    // The task's real reviewed/integrated lineage was never rewritten.
    let task = store.get_task(&record.task_id).unwrap().unwrap();
    assert_eq!(task.status, Status::Running);
    assert_eq!(
        task.repositories[0].reviewed_commit.as_deref(),
        Some("reviewed-r")
    );
    assert_eq!(
        task.repositories[0].integrated_commit.as_deref(),
        Some("delivered-m")
    );
    assert!(!coder.is_empty());
}

#[test]
fn completion_rejects_an_unverified_non_git_snapshot() {
    let (_temp, data, store) = fixture();
    let policy = StatePolicy::default();
    let record = finalizing(&store);
    store
        .save_non_git_target(&NonGitTarget {
            task_id: record.task_id.clone(),
            path: data.join("scripts"),
            snapshot_dir: data.join("super-agent").join("missing-snapshot"),
            snapshot_manifest_json: "{}".into(),
            diff_ref: Some("diff-1".into()),
            status: NonGitStatus::Completed,
            lock_key: format!("path:{}", data.join("scripts").display()),
            submission: None,
        })
        .unwrap();
    let unmet = gate_unmet(store.complete_task(&record.task_id, "complete", &policy));
    assert!(
        unmet
            .iter()
            .any(|item| matches!(item, Unmet::NonGitManifestInvalid { .. })),
        "{unmet:?}"
    );
}

fn non_git_spec(target: &std::path::Path) -> TaskSpec {
    TaskSpec {
        title: "Pinned non-Git delivery".into(),
        workspace: target.parent().unwrap().to_path_buf(),
        targets: vec![target.to_string_lossy().into_owned()],
        goal: "Deliver the reviewed bytes".into(),
        requirements: vec!["Keep the target readable".into()],
        acceptance_criteria: vec![AcceptanceCriterion {
            id: "AC-001".into(),
            description: "The reviewed file is delivered".into(),
        }],
        constraints: vec![],
        out_of_scope: vec![],
        target_branch: None,
        repository_target_branches: Default::default(),
    }
}

fn final_delivery_review(target: &std::path::Path, snapshot_ref: &str) -> Value {
    json!({
        "verdict": "APPROVED",
        "spec_version": 1,
        "round": 0,
        "stage": "FINAL_DELIVERY",
        "reviewed_versions": [{
            "repository_id": crate::super_agent::state::non_git_repository_id(target),
            "snapshot_ref": snapshot_ref
        }],
        "acceptance_results": [{
            "criterion": "AC-001",
            "passed": true,
            "evidence": ["compared the pinned payload with the delivered bytes"]
        }],
        "issues": [],
        "verification": ["read the pinned submission"],
        "limitations": []
    })
}

#[test]
fn completion_with_a_real_pinned_non_git_target_requires_the_reviewed_bytes() {
    let (temp, data, store) = fixture();
    let policy = StatePolicy::default();
    let target_dir = temp.path().join("scripts");
    std::fs::create_dir_all(&target_dir).unwrap();
    std::fs::write(target_dir.join("run.txt"), "value=1\n").unwrap();
    let target_dir = target_dir.canonicalize().unwrap();

    let task = store
        .create_task("router", non_git_spec(&target_dir), "create")
        .unwrap();
    let baseline = data
        .join("super-agent")
        .join(&task.task_id)
        .join("snapshots")
        .join("1");
    let manifest = crate::super_agent::isolation::create_snapshot(&target_dir, &baseline).unwrap();
    store
        .save_non_git_target(&NonGitTarget {
            task_id: task.task_id.clone(),
            path: target_dir.clone(),
            snapshot_dir: baseline,
            snapshot_manifest_json: serde_json::to_string(&manifest).unwrap(),
            diff_ref: None,
            status: NonGitStatus::Prepared,
            lock_key: format!("path:{}", target_dir.display()),
            submission: None,
        })
        .unwrap();

    let sessions = crate::runtime::SessionStore::new(store.db().clone());
    let coder = sessions.create(&target_dir, "fixture", "fixture").unwrap();
    let reviewer = sessions.create(&target_dir, "fixture", "fixture").unwrap();
    sessions
        .append(&coder, "user", "implement the task")
        .unwrap();
    sessions
        .append(&reviewer, "user", "review the task")
        .unwrap();
    store
        .set_sessions(&task.task_id, Some(&coder), Some(&reviewer))
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
        (
            EventType::ReviewCompleted,
            "review-final",
            json!({"verdict":"APPROVED","stage":"FINAL_DELIVERY"}),
        ),
        (EventType::HarnessAnalysisCompleted, "harness", json!({})),
    ] {
        store
            .apply_event(
                &task.task_id,
                event(event_type, operation, payload),
                &policy,
            )
            .unwrap();
    }
    let task = store.get_task(&task.task_id).unwrap().unwrap();
    assert_eq!(task.phase, Phase::Finalizing);

    // Pin the real submitted bytes through the production submission route.
    let initial = store.non_git_targets(&task.task_id).unwrap().remove(0);
    let mut pinned =
        crate::super_agent::review::record_non_git_changes(&store, &task, &initial).unwrap();
    pinned.status = NonGitStatus::Completed;
    store.save_non_git_target(&pinned).unwrap();
    let submission = pinned.submission.clone().expect("pinned submission");
    let payload = submission.directory.join("payload").join("run.txt");

    store
        .add_report(
            "implementation-report",
            &task.task_id,
            ReportKind::Implementation,
            1,
            0,
            json!({"spec_version":1,"round":0,"summary":"wrote the reviewed bytes"}),
        )
        .unwrap();
    store
        .add_report(
            "harness-report",
            &task.task_id,
            ReportKind::Harness,
            1,
            0,
            json!({"spec_version":1,"round":0,"findings":[]}),
        )
        .unwrap();
    store
        .add_report(
            "review-report",
            &task.task_id,
            ReportKind::Review,
            1,
            0,
            final_delivery_review(&target_dir, &submission.artifact_id),
        )
        .unwrap();

    // 1. A user edit after the review is not silently treated as approved and
    // is never overwritten.
    std::fs::write(target_dir.join("run.txt"), "user edit after review\n").unwrap();
    let unmet = gate_unmet(store.complete_task(&task.task_id, "complete-edited", &policy));
    let mismatch = unmet.iter().find_map(|item| match item {
        Unmet::NonGitManifestInvalid { reason, .. } => Some(reason.clone()),
        _ => None,
    });
    assert!(
        mismatch
            .as_deref()
            .is_some_and(|reason| reason.contains("no longer matches the reviewed submission")),
        "the gate must report the changed target: {unmet:?}"
    );
    let record = store.get_task(&task.task_id).unwrap().unwrap();
    assert_ne!(record.status, Status::Completed);
    assert_eq!(
        std::fs::read_to_string(target_dir.join("run.txt")).unwrap(),
        "user edit after review\n",
        "a rejected completion must not restore or erase user bytes"
    );
    assert_eq!(
        std::fs::read_to_string(&payload).unwrap(),
        "value=1\n",
        "the pinned artifact must stay unchanged"
    );

    // 2. Restoring the reviewed bytes is not enough if the review approved a
    // different snapshot identity.
    std::fs::write(target_dir.join("run.txt"), "value=1\n").unwrap();
    std::thread::sleep(std::time::Duration::from_millis(20));
    let stale = store
        .add_report(
            "review-stale",
            &task.task_id,
            ReportKind::Review,
            1,
            0,
            final_delivery_review(&target_dir, "non-git-older-submission"),
        )
        .unwrap();
    let unmet = gate_unmet(store.complete_task(&task.task_id, "complete-stale", &policy));
    assert!(
        unmet.iter().any(|item| matches!(
            item,
            Unmet::VersionMismatch { repository_id, .. }
                if repository_id == &crate::super_agent::state::non_git_repository_id(&target_dir)
        )),
        "a stale snapshot identity must not approve the current target: {unmet:?}"
    );
    assert_ne!(
        store.get_task(&task.task_id).unwrap().unwrap().status,
        Status::Completed
    );
    assert_ne!(stale.report_id, submission.artifact_id);

    // 3. The exact reviewed version and identity complete the real transition.
    std::thread::sleep(std::time::Duration::from_millis(20));
    store
        .add_report(
            "review-current",
            &task.task_id,
            ReportKind::Review,
            1,
            0,
            final_delivery_review(&target_dir, &submission.artifact_id),
        )
        .unwrap();
    let completed = store
        .complete_task(&task.task_id, "complete-valid", &policy)
        .unwrap();
    assert_eq!(completed.status, Status::Completed);
    assert_eq!(completed.phase, Phase::Finalizing);
    assert_eq!(
        std::fs::read_to_string(target_dir.join("run.txt")).unwrap(),
        "value=1\n"
    );
    assert!(store
        .events(&task.task_id)
        .unwrap()
        .iter()
        .any(|event| event.event_type == EventType::TaskCompleted));
}

fn separate_store(data: &std::path::Path) -> TaskStore {
    let db = crate::db::DbManager::new(data.join(crate::runtime::DATABASE_FILE)).unwrap();
    db.init().unwrap();
    TaskStore::new(Arc::new(db), data.to_path_buf())
}

#[test]
fn a_released_lock_operation_never_claims_current_ownership() {
    let (_temp, data, store) = fixture();
    let first = store.create_task("router", spec("A"), "create-a").unwrap();
    let second = store.create_task("router", spec("B"), "create-b").unwrap();
    let canonical = std::fs::canonicalize(&data).unwrap();
    let key = format!("path:{}", canonical.display());

    assert_eq!(
        store.try_lock("op-a", &key, &first.task_id).unwrap(),
        LockOutcome::Acquired
    );
    assert_eq!(
        store.try_lock("op-a", &key, &first.task_id).unwrap(),
        LockOutcome::AlreadyOwned
    );
    assert!(store.unlock(&key, &first.task_id, "op-a").unwrap());
    // The operation is remembered, but it no longer owns anything.
    assert_eq!(
        store.try_lock("op-a", &key, &first.task_id).unwrap(),
        LockOutcome::Released
    );
    assert_eq!(
        store.try_lock("op-b", &key, &second.task_id).unwrap(),
        LockOutcome::Acquired
    );
    assert_eq!(
        store.try_lock("op-a", &key, &first.task_id).unwrap(),
        LockOutcome::Released
    );
    assert!(store.locks(&first.task_id).unwrap().is_empty());
    assert_eq!(store.locks(&second.task_id).unwrap(), vec![key.clone()]);
    // A fresh operation must observe the current holder.
    assert!(matches!(
        store
            .try_lock("op-a-fresh", &key, &first.task_id)
            .unwrap_err(),
        StoreError::LockBusy(_)
    ));
    // A release must name the current owner; wrong owners leave it in place.
    assert!(!store.unlock(&key, &second.task_id, "op-a").unwrap());
    assert!(!store.unlock(&key, &first.task_id, "op-b").unwrap());
    assert_eq!(store.locks(&second.task_id).unwrap(), vec![key]);
}

#[test]
fn overlapping_path_locks_conflict_across_separate_connections() {
    let (_temp, data, store) = fixture();
    let second = separate_store(&data);
    let first_task = store
        .create_task("router", spec("Parent"), "create-parent")
        .unwrap();
    let second_task = store
        .create_task("router", spec("Child"), "create-child")
        .unwrap();

    let canonical = std::fs::canonicalize(&data).unwrap();
    let target = canonical.join("target");
    let child = target.join("sub");
    let sibling = canonical.join("target2");
    std::fs::create_dir(&target).unwrap();
    std::fs::create_dir(&child).unwrap();
    std::fs::create_dir(&sibling).unwrap();
    let key = |path: &std::path::Path| format!("path:{}", path.display());

    assert_eq!(
        store
            .try_lock("parent", &key(&target), &first_task.task_id)
            .unwrap(),
        LockOutcome::Acquired
    );
    // Ancestor/descendant and the same directory through a `.` alias conflict.
    assert!(matches!(
        second
            .try_lock("descendant", &key(&child), &second_task.task_id)
            .unwrap_err(),
        StoreError::LockBusy(_)
    ));
    assert!(matches!(
        second
            .try_lock("alias", &key(&target.join(".")), &second_task.task_id)
            .unwrap_err(),
        StoreError::LockBusy(_)
    ));
    // Sibling prefixes are independent paths, not string prefixes.
    assert_eq!(
        second
            .try_lock("sibling", &key(&sibling), &second_task.task_id)
            .unwrap(),
        LockOutcome::Acquired
    );
    // A second operation in the same task does not silently share the lock.
    assert!(matches!(
        store
            .try_lock("same-task", &key(&child), &first_task.task_id)
            .unwrap_err(),
        StoreError::LockBusy(_)
    ));

    assert!(store
        .unlock(&key(&target), &first_task.task_id, "parent")
        .unwrap());
    assert_eq!(
        store
            .try_lock("child-after-release", &key(&child), &first_task.task_id)
            .unwrap(),
        LockOutcome::Acquired
    );
    assert!(matches!(
        second
            .try_lock("parent-after-release", &key(&target), &second_task.task_id)
            .unwrap_err(),
        StoreError::LockBusy(_)
    ));
}

#[test]
fn concurrent_path_locks_on_separate_connections_allow_exactly_one_holder() {
    let (_temp, data, store) = fixture();
    let canonical = std::fs::canonicalize(&data).unwrap();
    let shared = canonical.join("shared");
    std::fs::create_dir(&shared).unwrap();
    let key = format!("path:{}", shared.display());
    let tasks: Vec<TaskRecord> = (0..4)
        .map(|index| {
            store
                .create_task("router", spec("Concurrent"), &format!("create-{index}"))
                .unwrap()
        })
        .collect();
    let stores: Vec<TaskStore> = (0..4).map(|_| separate_store(&data)).collect();

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(tasks.len()));
    let mut handles = Vec::new();
    for (index, (store, task)) in stores.into_iter().zip(tasks).enumerate() {
        let barrier = barrier.clone();
        let key = key.clone();
        handles.push(std::thread::spawn(move || {
            barrier.wait();
            store.try_lock(&format!("concurrent-{index}"), &key, &task.task_id)
        }));
    }
    let results: Vec<_> = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Ok(LockOutcome::Acquired)))
            .count(),
        1,
        "{results:?}"
    );
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(StoreError::LockBusy(_))))
            .count(),
        3,
        "{results:?}"
    );
}

#[test]
fn completion_rejects_an_empty_session_row_and_historical_round_events() {
    let (_temp, _data, store) = fixture();
    let policy = StatePolicy::default();
    let record = finalizing(&store);
    let coder = record.coder.session_id.clone().unwrap();
    let sessions = crate::runtime::SessionStore::new(store.db().clone());

    // Replace the coder with a fresh conversation row that has no messages.
    let empty = sessions
        .create(store.data_dir(), "fixture", "fixture")
        .unwrap();
    store
        .set_sessions(&record.task_id, Some(&empty), None)
        .unwrap();
    let unmet = gate_unmet(store.complete_task(&record.task_id, "complete-empty", &policy));
    assert!(
        unmet.iter().any(|item| matches!(
            item,
            Unmet::SessionUnverified {
                role: AgentRole::Coder
            }
        )),
        "{unmet:?}"
    );

    store
        .set_sessions(&record.task_id, Some(&coder), None)
        .unwrap();
    // Move the current contract to the next round; the existing events belong
    // to round 0 and must no longer satisfy the trace requirement.
    store
        .db()
        .with_conn(|conn| {
            conn.execute(
                "UPDATE sa_tasks SET review_round=1 WHERE task_id=?1",
                [&record.task_id],
            )
        })
        .unwrap()
        .unwrap();
    let unmet = gate_unmet(store.complete_task(&record.task_id, "complete-round", &policy));
    assert!(
        unmet.iter().any(|item| matches!(
            item,
            Unmet::TraceMissing {
                event: EventType::ImplementationSubmitted
            }
        )),
        "{unmet:?}"
    );
}

#[test]
fn ingested_events_stay_pending_until_processed_exactly_once() {
    let (_temp, _data, store) = fixture();
    let policy = StatePolicy::default();
    let task = store
        .create_task("router", spec("Pending"), "create")
        .unwrap();
    // The creation event is pending too; nothing has applied it yet.
    assert_eq!(
        store.get_task(&task.task_id).unwrap().unwrap().phase,
        Phase::Discovering
    );
    let ingested = store
        .ingest_event(
            &task.task_id,
            event(
                EventType::DiscoveryCompleted,
                "pending-discovery",
                json!({}),
            ),
        )
        .unwrap();
    assert!(ingested.inserted);
    assert!(!ingested.event.processed);
    assert_eq!(
        store.get_task(&task.task_id).unwrap().unwrap().phase,
        Phase::Discovering,
        "ingesting must not apply state"
    );
    // Re-ingesting the same operation is idempotent.
    let replay = store
        .ingest_event(
            &task.task_id,
            event(
                EventType::DiscoveryCompleted,
                "pending-discovery",
                json!({}),
            ),
        )
        .unwrap();
    assert!(!replay.inserted);
    assert_eq!(replay.event.event_id, ingested.event.event_id);

    let processed = store.process_pending("router", &policy).unwrap();
    assert_eq!(processed.len(), 1);
    let record = store.get_task(&task.task_id).unwrap().unwrap();
    assert_eq!(record.phase, Phase::Planning);
    assert!(store
        .events(&task.task_id)
        .unwrap()
        .iter()
        .all(|event| event.processed));
    // A second pass has nothing to do.
    assert!(store.process_pending("router", &policy).unwrap().is_empty());

    let mut applied = store
        .events(&task.task_id)
        .unwrap()
        .into_iter()
        .find(|event| event.operation_id == "pending-discovery")
        .unwrap();
    applied.processed = true;
    // Processing the same event directly is a no-op even if called again.
    assert!(store
        .process_event(&ingested.event.event_id, &policy)
        .unwrap()
        .is_none());
}

#[test]
fn a_pending_event_for_another_router_is_not_processed() {
    let (_temp, _data, store) = fixture();
    let policy = StatePolicy::default();
    let task = store
        .create_task("router-a", spec("Scoped"), "create")
        .unwrap();
    let other = store
        .create_task("router-b", spec("Other"), "create-other")
        .unwrap();
    store
        .ingest_event(
            &task.task_id,
            event(EventType::DiscoveryCompleted, "discovery-a", json!({})),
        )
        .unwrap();
    store
        .ingest_event(
            &other.task_id,
            event(EventType::DiscoveryCompleted, "discovery-b", json!({})),
        )
        .unwrap();
    let processed = store.process_pending("router-a", &policy).unwrap();
    assert_eq!(processed.len(), 1);
    assert_eq!(
        store.get_task(&task.task_id).unwrap().unwrap().phase,
        Phase::Planning
    );
    assert_eq!(
        store.get_task(&other.task_id).unwrap().unwrap().phase,
        Phase::Discovering
    );
}

#[test]
fn a_rejected_transition_keeps_earlier_pending_events_unapplied() {
    let (_temp, _data, store) = fixture();
    let policy = StatePolicy::default();
    let task = store
        .create_task("router", spec("Reject"), "create")
        .unwrap();
    // IMPLEMENTATION_SUBMITTED is illegal in DISCOVERING.
    store
        .ingest_event(
            &task.task_id,
            event(EventType::ImplementationSubmitted, "too-early", json!({})),
        )
        .unwrap();
    let error = store.process_pending("router", &policy).unwrap_err();
    assert!(matches!(error, StoreError::Transition(_)));
    assert_eq!(
        store.get_task(&task.task_id).unwrap().unwrap().phase,
        Phase::Discovering
    );
    assert!(store
        .events(&task.task_id)
        .unwrap()
        .iter()
        .filter(|event| event.event_type != EventType::TaskCreated)
        .all(|event| !event.processed));
}
