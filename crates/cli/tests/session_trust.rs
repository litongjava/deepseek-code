//! Trust recovery must work without a provider, Desktop, or a usable session.
use deepseek_code_core::{
    db::project_repo,
    runtime::{RuntimePaths, SessionStore},
};
use std::{
    path::Path,
    process::{Command, Output, Stdio},
};

fn cli(data: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dcl"))
        .args(["--data-dir", data.to_str().unwrap()])
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn startup_explains_and_explicit_command_repairs_without_model_credentials() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("中文 project 'quoted'");
    std::fs::create_dir(&source).unwrap();
    let paths = RuntimePaths::new(root.path().join("data")).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let id = store.create(&source, "fixture", "fixture").unwrap();
    let original = deepseek_code_core::services::session_trust::inspect(&store.db, &id).unwrap();
    store
        .db
        .with_conn(|conn| {
            conn.execute(
                "UPDATE project_folders SET trusted=0 WHERE project_id=?1",
                [&original.id],
            )
        })
        .unwrap()
        .unwrap();
    let startup = cli(&paths.data_dir, &["--resume", &id, "--print", "hello"]);
    assert!(!startup.status.success());
    let error = String::from_utf8_lossy(&startup.stderr);
    assert!(error.contains("sessions trust"), "{error}");
    assert!(error.contains("--folder"), "{error}");
    assert!(error.contains("not trusted"), "{error}");
    assert!(error.contains(&id), "{error}");
    assert!(error.contains("中文 project"), "{error}");
    assert!(!cli(&paths.data_dir, &["sessions", "trust", &id])
        .status
        .success());
    let repaired = cli(
        &paths.data_dir,
        &[
            "sessions",
            "trust",
            &id,
            "--folder",
            source.to_str().unwrap(),
        ],
    );
    assert!(
        repaired.status.success(),
        "{}",
        String::from_utf8_lossy(&repaired.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&repaired.stdout).unwrap();
    assert_eq!(result["session_id"], id);
    assert!(store.resume(&id).is_ok());
    store
        .db
        .with_conn(|conn| {
            assert!(
                !project_repo::get_project(conn, &original.id)
                    .unwrap()
                    .unwrap()
                    .folders[0]
                    .trusted
            )
        })
        .unwrap();
}

#[test]
fn repeated_folder_arguments_and_worker_rejection_preserve_scope() {
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("first");
    let second = root.path().join("second");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&second).unwrap();
    let paths = RuntimePaths::new(root.path().join("data")).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let id = store.create(&source, "fixture", "fixture").unwrap();
    let original = deepseek_code_core::services::session_trust::inspect(&store.db, &id).unwrap();
    store
        .db
        .with_conn(|conn| {
            project_repo::set_folders(
                conn,
                &original.id,
                &[
                    source.to_string_lossy().into_owned(),
                    second.to_string_lossy().into_owned(),
                ],
            )
        })
        .unwrap()
        .unwrap();
    let arguments = [
        "sessions",
        "trust",
        &id,
        "--folder",
        source.to_str().unwrap(),
        "--folder",
        second.to_str().unwrap(),
    ];
    std::fs::create_dir_all(paths.data_dir.join("background-tasks")).unwrap();
    let claim = paths
        .data_dir
        .join("background-tasks")
        .join(format!("session-{id}.claim"));
    std::fs::write(&claim, "active-worker-fixture").unwrap();
    let result = cli(&paths.data_dir, &arguments);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("worker owns"));
    assert_eq!(
        std::fs::read_to_string(&claim).unwrap(),
        "active-worker-fixture"
    );
    std::fs::remove_file(&claim).unwrap();
    let result = cli(&paths.data_dir, &arguments);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        deepseek_code_core::services::session_trust::inspect(&store.db, &id)
            .unwrap()
            .folders
            .len(),
        2
    );
}

#[test]
fn standalone_shutdown_refuses_owned_shells_and_explicit_stop_is_scoped() {
    let root = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(root.path().join("data")).unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    let other = uuid::Uuid::new_v4().to_string();
    let task = deepseek_code_core::tools::tasks::execute_for_session(
        "TaskCreate",
        serde_json::json!({"subject":"managed fixture"}),
        &paths.data_dir,
        &id,
    )
    .unwrap();
    let task_id = task["id"].as_str().unwrap();
    // Fixture persists an owner-created shell record; do not spawn a real shell.
    let mut task = task.clone();
    task["metadata"] = serde_json::json!({"managed_shell":true});
    task["status"] = serde_json::json!("running");
    let conn =
        deepseek_code_core::db::DbManager::new(paths.data_dir.join("tasks").join(format!("{id}.sqlite3")))
            .unwrap();
    conn.with_conn(|c| {
        c.execute(
            "UPDATE tasks SET body=?1 WHERE id=?2",
            [task.to_string(), task_id.to_owned()],
        )
    })
    .unwrap()
    .unwrap();
    let shell = paths.data_dir.join("shell-tasks").join(&id).join(task_id);
    std::fs::create_dir_all(&shell).unwrap();
    std::fs::write(shell.join("heartbeat"), "running").unwrap();
    let result = cli(&paths.data_dir, &["sessions", "shutdown-worker", &id]);
    assert!(!result.status.success());
    let error = String::from_utf8_lossy(&result.stderr);
    assert!(
        error.contains(task_id) && error.contains("sessions stop-task"),
        "{error}"
    );
    assert!(!shell.join("cancel").exists());
    let wrong = cli(&paths.data_dir, &["sessions", "stop-task", &other, task_id]);
    assert!(!wrong.status.success());
    assert!(!shell.join("cancel").exists());
    let stop = cli(&paths.data_dir, &["sessions", "stop-task", &id, task_id]);
    assert!(
        stop.status.success(),
        "{}",
        String::from_utf8_lossy(&stop.stderr)
    );
    assert!(shell.join("cancel").exists());
    // A cancellation request is not completion: shutdown remains blocked.
    assert!(!cli(&paths.data_dir, &["sessions", "shutdown-worker", &id])
        .status
        .success());
    assert!(
        cli(&paths.data_dir, &["sessions", "shutdown-worker", &other])
            .status
            .success()
    );
}
