use super::*;
use crate::services::ServiceContext;
use crate::super_agent::crp::ToolRuntime;
use crate::super_agent::model::{NewEvent, RepoTarget, TaskSpec};
use crate::super_agent::store::TaskStore;
use crate::super_agent::{EventType, Phase, Status};
use serde_json::json;
use std::sync::Arc;

struct Fixture {
    _temp: tempfile::TempDir,
    data: std::path::PathBuf,
    project: std::path::PathBuf,
    context: ServiceContext,
    store: TaskStore,
    router: String,
    coder: String,
    reviewer: String,
    outsider: String,
    task_id: String,
}

impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        let data = temp.path().join("data");
        let context = ServiceContext::open(
            crate::runtime::RuntimePaths::new(data.clone()).unwrap(),
            Arc::new(crate::services::EnvironmentCredentials),
        )
        .unwrap();
        let sessions = crate::runtime::SessionStore::new(context.db.clone());
        let router = sessions.create(&project, "fixture", "fixture").unwrap();
        let coder = sessions.create(&project, "fixture", "fixture").unwrap();
        let reviewer = sessions.create(&project, "fixture", "fixture").unwrap();
        let outsider = sessions.create(&project, "fixture", "fixture").unwrap();
        let store = TaskStore::new(context.db.clone(), data.clone());
        let spec = TaskSpec {
            title: "Review fixture".into(),
            workspace: project.clone(),
            targets: vec![project.to_string_lossy().into_owned()],
            goal: "Review the fixture".into(),
            requirements: vec![],
            acceptance_criteria: vec![],
            constraints: vec![],
            out_of_scope: vec![],
            target_branch: None,
            repository_target_branches: Default::default(),
        };
        let task = store.create_task(&router, spec, "create").unwrap();
        store
            .set_sessions(&task.task_id, Some(&coder), Some(&reviewer))
            .unwrap();
        Self {
            _temp: temp,
            data,
            project,
            context,
            store,
            router,
            coder,
            reviewer,
            outsider,
            task_id: task.task_id,
        }
    }

    fn task(&self) -> crate::super_agent::TaskRecord {
        self.store.get_task(&self.task_id).unwrap().unwrap()
    }

    fn runtime(&self, session: &str) -> ToolRuntime {
        ToolRuntime {
            db: self.context.db.clone(),
            data_dir: self.data.clone(),
            session_id: session.into(),
        }
    }
}

#[test]
fn reviewer_definitions_are_scoped_and_typed_submission_is_published() {
    let names: Vec<String> = crate::super_agent::crp::definitions(
        crate::super_agent::Role::Agent,
        Some(crate::super_agent::MemberRole::Reviewer),
    )
    .into_iter()
    .map(|definition| definition.name)
    .collect();
    for expected in [
        "SessionMetadata",
        "SessionMessages",
        "SessionEvents",
        "SessionChildren",
        "ArtifactRead",
    ] {
        assert!(names.contains(&expected.to_string()), "{names:?}");
    }
    let coder: Vec<String> = crate::super_agent::crp::definitions(
        crate::super_agent::Role::Agent,
        Some(crate::super_agent::MemberRole::Coder),
    )
    .into_iter()
    .map(|definition| definition.name)
    .collect();
    assert!(!coder.contains(&"SessionMessages".to_string()));
    let submit = crate::super_agent::crp::definitions(
        crate::super_agent::Role::Agent,
        Some(crate::super_agent::MemberRole::Coder),
    )
    .into_iter()
    .find(|definition| definition.name == "CrpSubmit")
    .unwrap();
    assert!(submit.input_schema["properties"]["payload"]["oneOf"]
        .as_array()
        .is_some_and(|schemas| schemas.len() == 5));
}

#[test]
fn review_request_binds_current_versions_sessions_events_and_repo_commits() {
    let fixture = Fixture::new();
    let task = fixture.task();
    fixture
        .store
        .ingest_event(
            &fixture.task_id,
            NewEvent {
                session_id: Some(fixture.coder.clone()),
                run_id: None,
                event_type: EventType::PlanSubmitted,
                causation_id: None,
                operation_id: "plan-event".into(),
                payload: json!({}),
            },
        )
        .unwrap();
    let child = uuid::Uuid::new_v4().to_string();
    crate::services::subagents::save_invocation(
        &fixture.context,
        &crate::services::subagents::Invocation {
            parent_session_id: fixture.coder.clone(),
            parent_message_id: "message".into(),
            tool_use_id: "tool".into(),
            child_session_id: child.clone(),
            attempt_id: "attempt".into(),
            mode: "background".into(),
            status: "completed".into(),
            description: "child".into(),
        },
    )
    .unwrap();
    let repo = RepoTarget {
        task_id: fixture.task_id.clone(),
        repository_id: "repo-1".into(),
        repo_root: fixture.project.clone(),
        git_common_dir: fixture.project.clone(),
        target_path: fixture.project.clone(),
        target_branch: "main".into(),
        task_branch: "agent/task".into(),
        worktree_path: fixture.project.clone(),
        working_path: fixture.project.clone(),
        base_commit: "base".into(),
        submitted_commit: Some("submitted".into()),
        reviewed_commit: None,
        integrated_commit: None,
        integration_status: Default::default(),
        verification_status: Default::default(),
        source_state: vec![],
    };
    fixture.store.save_repo(&repo).unwrap();
    let task = fixture.task();
    assert_eq!(task.phase, Phase::Discovering);
    let request = build_review_request(&fixture.store, &task).unwrap();
    let request = &request["review_request"];
    assert_eq!(request["task_id"], fixture.task_id);
    assert_eq!(request["spec_version"], task.spec_version);
    assert_eq!(request["review_round"], task.review_round);
    assert_eq!(request["coder"]["session_id"], fixture.coder);
    assert!(request["coder"]["child_session_ids"]
        .as_array()
        .unwrap()
        .contains(&json!(child)));
    assert_eq!(
        request["repository_versions"][0]["submitted_commit"],
        "submitted"
    );
    assert!(request["runtime"]["harness_version"]
        .as_str()
        .is_some_and(|version| !version.is_empty()));
    assert!(request["trace"]["end_sequence"].as_u64().unwrap() >= 1);
}

#[test]
fn reviewer_queries_page_redact_and_reject_out_of_scope_sessions() {
    let fixture = Fixture::new();
    let sessions = crate::runtime::SessionStore::new(fixture.context.db.clone());
    let mut ids = Vec::new();
    for index in 0..5 {
        ids.push(
            sessions
                .append(&fixture.coder, "assistant", &format!("message {index}"))
                .unwrap(),
        );
    }
    let secret_message = sessions
        .append(&fixture.coder, "user", "api_key=abcdef123456")
        .unwrap();
    ids.push(secret_message.clone());
    fixture
        .context
        .db
        .with_conn(|conn| {
            crate::db::message_repo::insert_tool_call(
                conn,
                "call-1",
                &secret_message,
                "Bash",
                Some(r#"{"command":"echo sk-abcdefghijklmnop"}"#),
                Some("token ghp_abcdefghijklmnopqrst"),
                false,
                0,
            )
        })
        .unwrap()
        .unwrap();
    let runtime = fixture.runtime(&fixture.reviewer);

    let metadata = execute(
        &runtime,
        "SessionMetadata",
        json!({"session_id": fixture.coder}),
    )
    .unwrap();
    assert_eq!(metadata["message_count"], 6);
    assert_eq!(metadata["tool_call_count"], 1);

    let first = execute(
        &runtime,
        "SessionMessages",
        json!({"session_id": fixture.coder, "limit": 2}),
    )
    .unwrap();
    assert_eq!(first["messages"].as_array().unwrap().len(), 2);
    let cursor = first["next_cursor"].as_str().unwrap().to_owned();
    let second = execute(
        &runtime,
        "SessionMessages",
        json!({"session_id": fixture.coder, "limit": 2, "cursor": cursor}),
    )
    .unwrap();
    assert_eq!(second["messages"].as_array().unwrap().len(), 2);
    let cursor = second["next_cursor"].as_str().unwrap().to_owned();
    let third = execute(
        &runtime,
        "SessionMessages",
        json!({"session_id": fixture.coder, "limit": 2, "cursor": cursor}),
    )
    .unwrap();
    assert_eq!(third["messages"].as_array().unwrap().len(), 2);
    let serialized = third.to_string();
    assert!(!serialized.contains("abcdef123456"), "{serialized}");
    assert!(!serialized.contains("ghp_abcdefghijklmnopqrst"), "{serialized}");
    assert!(!serialized.contains("sk-abcdefghijklmnop"), "{serialized}");
    assert!(third["next_cursor"].is_null());

    let denied = execute(
        &runtime,
        "SessionMessages",
        json!({"session_id": fixture.outsider}),
    )
    .unwrap_err();
    assert!(denied.to_string().contains("not part of this task"));
    assert!(execute(&fixture.runtime(&fixture.coder), "SessionMetadata", json!({"session_id":fixture.coder}))
        .unwrap_err()
        .to_string()
        .contains("only available to the task Reviewer"));
}

#[test]
fn artifacts_are_reports_or_regular_task_files_only() {
    let fixture = Fixture::new();
    let task = fixture.task();
    let runtime = fixture.runtime(&fixture.reviewer);
    let report = fixture
        .store
        .add_report(
            "report-op",
            &task.task_id,
            crate::super_agent::ReportKind::Plan,
            task.spec_version,
            task.review_round,
            json!({
                "spec_version": task.spec_version,
                "round": task.review_round,
                "understanding": "done"
            }),
        )
        .unwrap();
    let read = execute(
        &runtime,
        "ArtifactRead",
        json!({"artifact_id": report.report_id}),
    )
    .unwrap();
    assert_eq!(read["kind"], "report");
    assert_eq!(read["body"]["understanding"], "done");

    fixture
        .store
        .write_artifact(&task.task_id, "changes.json", b"{\"changed\":1}")
        .unwrap();
    let read = execute(&runtime, "ArtifactRead", json!({"artifact_id":"changes.json"})).unwrap();
    assert_eq!(read["kind"], "file");
    assert_eq!(read["content"], "{\"changed\":1}");
    assert!(execute(&runtime, "ArtifactRead", json!({"artifact_id":"../escape"}))
        .is_err());
    assert!(execute(&runtime, "ArtifactRead", json!({"artifact_id":"missing"}))
        .is_err());

    #[cfg(unix)]
    {
        let outside = fixture._temp.path().join("outside.txt");
        std::fs::write(&outside, "secret").unwrap();
        let artifacts = fixture
            .data
            .join("super-agent")
            .join(&task.task_id)
            .join("artifacts");
        std::os::unix::fs::symlink(&outside, artifacts.join("link")).unwrap();
        let error = execute(&runtime, "ArtifactRead", json!({"artifact_id":"link"})).unwrap_err();
        assert!(error.to_string().contains("regular file"), "{error:#}");
    }
}

#[test]
fn session_children_and_events_are_scoped() {
    let fixture = Fixture::new();
    let child = uuid::Uuid::new_v4().to_string();
    crate::services::subagents::save_invocation(
        &fixture.context,
        &crate::services::subagents::Invocation {
            parent_session_id: fixture.coder.clone(),
            parent_message_id: "message".into(),
            tool_use_id: "tool".into(),
            child_session_id: child.clone(),
            attempt_id: "attempt".into(),
            mode: "background".into(),
            status: "running".into(),
            description: "child".into(),
        },
    )
    .unwrap();
    fixture
        .store
        .ingest_event(
            &fixture.task_id,
            NewEvent {
                session_id: Some(fixture.coder.clone()),
                run_id: None,
                event_type: EventType::PlanSubmitted,
                causation_id: None,
                operation_id: "plan-event".into(),
                payload: json!({}),
            },
        )
        .unwrap();
    let runtime = fixture.runtime(&fixture.reviewer);
    let children = execute(&runtime, "SessionChildren", json!({"session_id":fixture.coder}))
        .unwrap();
    assert_eq!(children["children"][0]["session_id"], child);
    let events = execute(
        &runtime,
        "SessionEvents",
        json!({"session_id": fixture.coder, "type":"PLAN_SUBMITTED"}),
    )
    .unwrap();
    assert_eq!(events["events"].as_array().unwrap().len(), 1);
    assert_eq!(events["events"][0]["event_type"], "PLAN_SUBMITTED");
    let none = execute(
        &runtime,
        "SessionEvents",
        json!({"session_id": child, "type":"PLAN_SUBMITTED"}),
    )
    .unwrap();
    assert!(none["events"].as_array().unwrap().is_empty());
}

#[test]
fn non_git_review_copy_is_independent_of_later_changes() {
    let fixture = Fixture::new();
    let target_dir = fixture._temp.path().join("non-git");
    std::fs::create_dir_all(&target_dir).unwrap();
    std::fs::write(target_dir.join("submitted.txt"), "submitted\n").unwrap();
    let target = crate::super_agent::NonGitTarget {
        task_id: fixture.task_id.clone(),
        path: target_dir.clone(),
        snapshot_dir: fixture._temp.path().join("original-snapshot"),
        snapshot_manifest_json: "{}".into(),
        diff_ref: None,
        status: crate::super_agent::NonGitStatus::Prepared,
        lock_key: "lock".into(),
    };
    let review = prepare_non_git_review(&fixture.data, &fixture.task_id, 0, &target).unwrap();
    assert_eq!(
        std::fs::read_to_string(review.join("submitted.txt")).unwrap(),
        "submitted\n"
    );
    std::fs::write(target_dir.join("submitted.txt"), "later\n").unwrap();
    assert_eq!(
        std::fs::read_to_string(review.join("submitted.txt")).unwrap(),
        "submitted\n"
    );
}
