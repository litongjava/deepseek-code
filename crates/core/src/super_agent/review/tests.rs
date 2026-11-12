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
    let target = prepared_target(&fixture, &target_dir, "original-snapshot");
    let submitted = record_non_git_changes(&fixture.store, &fixture.task(), &target).unwrap();
    let review = prepare_non_git_review(&fixture.data, &fixture.task_id, 0, &submitted).unwrap();
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

fn prepared_target(
    fixture: &Fixture,
    directory: &std::path::Path,
    baseline_name: &str,
) -> crate::super_agent::NonGitTarget {
    let baseline = fixture._temp.path().join(baseline_name);
    let manifest = crate::super_agent::isolation::create_snapshot(directory, &baseline).unwrap();
    crate::super_agent::NonGitTarget {
        task_id: fixture.task_id.clone(),
        path: directory.canonicalize().unwrap(),
        snapshot_dir: baseline,
        snapshot_manifest_json: serde_json::to_string(&manifest).unwrap(),
        diff_ref: None,
        status: crate::super_agent::NonGitStatus::Prepared,
        lock_key: "lock".into(),
        submission: None,
    }
}

// Reproduction for round08 requirement 1: two distinct authorized roots whose
// final path component is identical must not share a review snapshot.
#[test]
fn non_git_review_keeps_same_basename_targets_separate() {
    let fixture = Fixture::new();
    let first = fixture._temp.path().join("alpha/scripts");
    let second = fixture._temp.path().join("beta/scripts");
    std::fs::create_dir_all(&first).unwrap();
    std::fs::create_dir_all(&second).unwrap();
    std::fs::write(first.join("same.txt"), "first root\n").unwrap();
    std::fs::write(second.join("same.txt"), "second root\n").unwrap();
    let first_target = prepared_target(&fixture, &first, "first-baseline");
    let second_target = prepared_target(&fixture, &second, "second-baseline");
    let first_target =
        record_non_git_changes(&fixture.store, &fixture.task(), &first_target).unwrap();
    let second_target =
        record_non_git_changes(&fixture.store, &fixture.task(), &second_target).unwrap();

    let first_review =
        prepare_non_git_review(&fixture.data, &fixture.task_id, 0, &first_target).unwrap();
    let second_review =
        prepare_non_git_review(&fixture.data, &fixture.task_id, 0, &second_target).unwrap();

    assert_ne!(first_review, second_review);
    assert_eq!(
        std::fs::read_to_string(first_review.join("same.txt")).unwrap(),
        "first root\n"
    );
    assert_eq!(
        std::fs::read_to_string(second_review.join("same.txt")).unwrap(),
        "second root\n"
    );
}

// Reproduction for round08 requirement 3: the review must read the submission
// bytes captured at submission time, not whatever the original contains when
// the Reviewer starts.
#[test]
fn non_git_review_reads_submission_snapshot_not_later_edits() {
    let fixture = Fixture::new();
    let target_dir = fixture._temp.path().join("non-git");
    std::fs::create_dir_all(&target_dir).unwrap();
    std::fs::write(target_dir.join("submitted.txt"), "submitted\n").unwrap();
    let target = prepared_target(&fixture, &target_dir, "original-snapshot");
    let submitted =
        record_non_git_changes(&fixture.store, &fixture.task(), &target).unwrap();
    std::fs::write(target_dir.join("submitted.txt"), "edited later\n").unwrap();

    let review =
        prepare_non_git_review(&fixture.data, &fixture.task_id, 0, &submitted).unwrap();
    assert_eq!(
        std::fs::read_to_string(review.join("submitted.txt")).unwrap(),
        "submitted\n"
    );
}

fn published_submissions(fixture: &Fixture) -> Vec<std::ffi::OsString> {
    let submissions = fixture
        .data
        .join("super-agent")
        .join(&fixture.task_id)
        .join("submissions");
    std::fs::read_dir(&submissions)
        .map(|entries| {
            entries
                .map(|entry| entry.unwrap().file_name())
                .filter(|name| !name.to_string_lossy().starts_with(".staging-"))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn non_git_binary_bytes_and_modes_are_pinned() {
    let fixture = Fixture::new();
    let target_dir = fixture._temp.path().join("non-git");
    std::fs::create_dir_all(&target_dir).unwrap();
    let binary = [0u8, 1, 2, 255, 128, 0, 65];
    std::fs::write(target_dir.join("data.bin"), binary).unwrap();
    std::fs::write(target_dir.join("run.sh"), "#!/bin/sh\nexit 0\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            target_dir.join("run.sh"),
            std::fs::Permissions::from_mode(0o751),
        )
        .unwrap();
    }
    let target = prepared_target(&fixture, &target_dir, "baseline");
    let submitted = record_non_git_changes(&fixture.store, &fixture.task(), &target).unwrap();
    let review = prepare_non_git_review(&fixture.data, &fixture.task_id, 0, &submitted).unwrap();
    assert_eq!(std::fs::read(review.join("data.bin")).unwrap(), binary);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            std::fs::metadata(review.join("run.sh")).unwrap().mode() & 0o7777,
            0o751
        );
    }
    let submission = submitted.submission.clone().unwrap();
    assert_eq!(submission.content_hash.len(), 64);
    assert!(submission.content_hash.bytes().all(|byte| byte.is_ascii_hexdigit()));
}

#[cfg(unix)]
#[test]
fn non_git_submission_rejects_symlinks_that_escape_the_pinned_payload() {
    // An absolute link that pointed inside the original would be copied as an
    // absolute path and later follow the mutable original. It must be rejected.
    let fixture = Fixture::new();
    let target_dir = fixture._temp.path().join("non-git");
    std::fs::create_dir_all(&target_dir).unwrap();
    std::fs::write(target_dir.join("real.txt"), "original\n").unwrap();
    std::os::unix::fs::symlink(target_dir.join("real.txt"), target_dir.join("absolute.txt"))
        .unwrap();
    let target = prepared_target(&fixture, &target_dir, "absolute-baseline");
    let error = record_non_git_changes(&fixture.store, &fixture.task(), &target)
        .expect_err("absolute symlink must be rejected");
    assert!(format!("{error:#}").contains("absolute"), "{error:#}");
    assert!(published_submissions(&fixture).is_empty());

    // A relative link chain that leaves the payload is equally unusable.
    let second = fixture._temp.path().join("relative/non-git");
    std::fs::create_dir_all(&second).unwrap();
    std::fs::write(
        fixture._temp.path().join("relative/outside.txt"),
        "outside\n",
    )
    .unwrap();
    std::os::unix::fs::symlink("../outside.txt", second.join("escape.txt")).unwrap();
    let target = prepared_target(&fixture, &second, "relative-baseline");
    let error = record_non_git_changes(&fixture.store, &fixture.task(), &target)
        .expect_err("escaping relative symlink must be rejected");
    assert!(format!("{error:#}").contains("escapes"), "{error:#}");
    assert!(published_submissions(&fixture).is_empty());
}

#[cfg(unix)]
#[test]
fn non_git_pinned_payload_rejects_unbound_extra_mode_and_type_changes() {
    let fixture = Fixture::new();
    let target_dir = fixture._temp.path().join("non-git");
    std::fs::create_dir_all(&target_dir).unwrap();
    std::fs::write(target_dir.join("run.sh"), "#!/bin/sh\nexit 0\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        target_dir.join("run.sh"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    let target = prepared_target(&fixture, &target_dir, "baseline");
    let submitted =
        record_non_git_changes(&fixture.store, &fixture.task(), &target).unwrap();
    let payload = submitted
        .submission
        .as_ref()
        .unwrap()
        .directory
        .join("payload");

    // Extra payload entry not bound by the manifest.
    std::fs::write(payload.join("extra.txt"), "extra\n").unwrap();
    let error = prepare_non_git_review(&fixture.data, &fixture.task_id, 0, &submitted)
        .expect_err("extra payload entry must be rejected");
    assert!(format!("{error:#}").contains("not bound"), "{error:#}");
    std::fs::remove_file(payload.join("extra.txt")).unwrap();

    // Executable bit removed from the payload copy.
    std::fs::set_permissions(
        payload.join("run.sh"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    let error = prepare_non_git_review(&fixture.data, &fixture.task_id, 0, &submitted)
        .expect_err("mode change must be rejected");
    assert!(format!("{error:#}").contains("permission"), "{error:#}");
    std::fs::set_permissions(
        payload.join("run.sh"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();

    // A listed regular file replaced by a symlink (even to in-payload bytes).
    let outside = fixture._temp.path().join("outside.txt");
    std::fs::write(&outside, "outside\n").unwrap();
    std::fs::remove_file(payload.join("run.sh")).unwrap();
    std::os::unix::fs::symlink(&outside, payload.join("run.sh")).unwrap();
    let error = prepare_non_git_review(&fixture.data, &fixture.task_id, 0, &submitted)
        .expect_err("file to symlink replacement must be rejected");
    let message = format!("{error:#}");
    assert!(
        message.contains("file type") || message.contains("absolute"),
        "{message}"
    );
}

#[cfg(unix)]
#[test]
fn non_git_submission_preserves_nested_directory_modes() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let target_dir = fixture._temp.path().join("non-git");
    let nested = target_dir.join("private/inner");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(nested.join("data.bin"), [0u8, 7, 255]).unwrap();
    std::fs::write(target_dir.join("private/notes.txt"), "notes\n").unwrap();
    std::fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::set_permissions(
        target_dir.join("private"),
        std::fs::Permissions::from_mode(0o750),
    )
    .unwrap();

    let target = prepared_target(&fixture, &target_dir, "baseline");
    let submitted = record_non_git_changes(&fixture.store, &fixture.task(), &target).unwrap();
    let review = prepare_non_git_review(&fixture.data, &fixture.task_id, 0, &submitted).unwrap();
    let mode = |path: &std::path::Path| {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
    };
    assert_eq!(mode(&review.join("private")), 0o750);
    assert_eq!(mode(&review.join("private/inner")), 0o700);
    assert_eq!(
        std::fs::read(review.join("private/inner/data.bin")).unwrap(),
        [0u8, 7, 255]
    );
    assert_eq!(
        std::fs::read_to_string(review.join("private/notes.txt")).unwrap(),
        "notes\n"
    );
}

#[test]
fn non_git_submission_retry_reuses_published_metadata() {
    let fixture = Fixture::new();
    let target_dir = fixture._temp.path().join("non-git");
    std::fs::create_dir_all(&target_dir).unwrap();
    std::fs::write(target_dir.join("submitted.txt"), "submitted\n").unwrap();
    let target = prepared_target(&fixture, &target_dir, "baseline");
    let first = record_non_git_changes(&fixture.store, &fixture.task(), &target).unwrap();
    let original = first.submission.clone().unwrap();

    // Simulate a crash after publishing the artifact but before the durable
    // target record saved the submission: replay the original target.
    let retry = record_non_git_changes(&fixture.store, &fixture.task(), &target).unwrap();
    let reused = retry.submission.clone().unwrap();
    assert_eq!(reused.artifact_id, original.artifact_id);
    assert_eq!(reused.directory, original.directory);
    assert_eq!(reused.created_at, original.created_at);
    assert_eq!(published_submissions(&fixture).len(), 1);
    prepare_non_git_review(&fixture.data, &fixture.task_id, 0, &retry).unwrap();
}

#[test]
fn non_git_submission_identity_is_round_specific_and_survives_reopen() {
    let fixture = Fixture::new();
    let target_dir = fixture._temp.path().join("non-git");
    std::fs::create_dir_all(&target_dir).unwrap();
    std::fs::write(target_dir.join("submitted.txt"), "round zero\n").unwrap();
    let target = prepared_target(&fixture, &target_dir, "baseline");
    let first = record_non_git_changes(&fixture.store, &fixture.task(), &target).unwrap();
    let first_submission = first.submission.clone().unwrap();

    let reopened = fixture
        .store
        .non_git_targets(&fixture.task_id)
        .unwrap()
        .into_iter()
        .find(|candidate| candidate.path == first.path)
        .unwrap();
    assert_eq!(reopened.submission, Some(first_submission.clone()));
    prepare_non_git_review(&fixture.data, &fixture.task_id, 0, &reopened).unwrap();

    std::fs::write(target_dir.join("submitted.txt"), "round one\n").unwrap();
    let mut next_task = fixture.task();
    next_task.review_round = 1;
    let second = record_non_git_changes(&fixture.store, &next_task, &first).unwrap();
    let second_submission = second.submission.clone().unwrap();
    assert_ne!(second_submission.artifact_id, first_submission.artifact_id);
    assert_ne!(second_submission.directory, first_submission.directory);
    prepare_non_git_review(&fixture.data, &fixture.task_id, 1, &second).unwrap();
    // The old artifact remains inspectable after the new submission.
    let old_review =
        prepare_non_git_review(&fixture.data, &fixture.task_id, 0, &first).unwrap();
    assert_eq!(
        std::fs::read_to_string(old_review.join("submitted.txt")).unwrap(),
        "round zero\n"
    );
}

#[test]
fn non_git_submission_blocks_missing_or_corrupt_snapshot() {
    let fixture = Fixture::new();
    let target_dir = fixture._temp.path().join("non-git");
    std::fs::create_dir_all(&target_dir).unwrap();
    std::fs::write(target_dir.join("submitted.txt"), "submitted\n").unwrap();
    let target = prepared_target(&fixture, &target_dir, "baseline");
    let submitted =
        record_non_git_changes(&fixture.store, &fixture.task(), &target).unwrap();
    let directory = submitted.submission.as_ref().unwrap().directory.clone();

    let manifest = directory.join("manifest.json");
    let saved = fixture._temp.path().join("saved-manifest.json");
    std::fs::rename(&manifest, &saved).unwrap();
    let error = prepare_non_git_review(&fixture.data, &fixture.task_id, 0, &submitted)
        .expect_err("missing manifest must block");
    assert!(format!("{error:#}").contains("manifest"), "{error:#}");
    std::fs::rename(&saved, &manifest).unwrap();

    std::fs::remove_file(directory.join("payload/submitted.txt")).unwrap();
    let error = prepare_non_git_review(&fixture.data, &fixture.task_id, 0, &submitted)
        .expect_err("missing payload must block");
    assert!(format!("{error:#}").contains("missing"), "{error:#}");
}

#[test]
fn edited_target_no_longer_matches_reviewed_submission() {
    let fixture = Fixture::new();
    let target_dir = fixture._temp.path().join("non-git");
    std::fs::create_dir_all(&target_dir).unwrap();
    std::fs::write(target_dir.join("submitted.txt"), "submitted\n").unwrap();
    let target = prepared_target(&fixture, &target_dir, "baseline");
    let submitted =
        record_non_git_changes(&fixture.store, &fixture.task(), &target).unwrap();
    let task = fixture.task();
    let manifest = verify_pinned_submission(&fixture.data, &task, &submitted).unwrap();
    assert!(submission_matches_target(&submitted, &manifest).unwrap());

    std::fs::write(target_dir.join("submitted.txt"), "edited after review\n").unwrap();
    assert!(!submission_matches_target(&submitted, &manifest).unwrap());
    // Reviewing the pinned bytes still works; only delivery is blocked.
    prepare_non_git_review(&fixture.data, &fixture.task_id, 0, &submitted).unwrap();
}
