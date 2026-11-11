//! Real `dcl tasks import-plan` runs against isolated legacy and data
//! directories. No provider credentials or user databases are used.
//!
//! The legacy workspace database is written directly by these fixtures. The
//! retired `plan_mode` tool is never called, so the fixture cannot depend on a
//! callable production entry that no longer exists.
use deepseek_code_core::tools::tasks;
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    process::Command,
};

const SESSION: &str = "11111111-2222-4333-8444-555555555555";

struct Output {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run(args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dcl"));
    command.args(args);
    for (key, _) in std::env::vars() {
        if key.starts_with("DEEPSEEK_CODE") {
            command.env_remove(key);
        }
    }
    let output = command.output().expect("running dcl");
    Output {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn report(output: &Output) -> Value {
    serde_json::from_str(output.stdout.trim()).unwrap_or_else(|error| {
        panic!("stdout is not one JSON report ({error}): {}", output.stdout)
    })
}

fn workspace(root: &Path) -> PathBuf {
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    workspace
}

fn legacy_db(workspace: &Path) -> Connection {
    let claude = workspace.join(".claude");
    std::fs::create_dir_all(&claude).unwrap();
    let db = Connection::open(claude.join("coordination.sqlite3")).unwrap();
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS records(kind TEXT NOT NULL, id TEXT NOT NULL, body TEXT NOT NULL, \
           PRIMARY KEY(kind,id)); \
         CREATE TABLE IF NOT EXISTS current_plan(singleton INTEGER PRIMARY KEY CHECK(singleton=1), \
           id TEXT NOT NULL);",
    )
    .unwrap();
    db
}

fn source_bytes(workspace: &Path) -> Vec<u8> {
    std::fs::read(workspace.join(".claude/coordination.sqlite3")).unwrap()
}

/// Writes the same legacy record shape the old project-plan tool produced.
fn create_plan(workspace: &Path, steps: Value) -> String {
    let plan_id = uuid::Uuid::new_v4().to_string();
    let steps: Vec<Value> = steps
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .map(|(index, step)| {
            json!({
                "step_id": format!("step-{index}"),
                "description": step["description"],
                "dependencies": step.get("dependencies").cloned().unwrap_or(json!([])),
                "status": "pending",
                "result": null
            })
        })
        .collect();
    let body = json!({
        "plan_id": plan_id,
        "task": "Migrate the plan",
        "steps": steps,
        "status": "draft",
        "created_at": "2026-01-01T00:00:00+00:00",
        "updated_at": "2026-01-02T00:00:00+00:00"
    });
    legacy_db(workspace)
        .execute(
            "INSERT INTO records(kind,id,body) VALUES('plan',?1,?2)",
            params![plan_id, body.to_string()],
        )
        .unwrap();
    plan_id
}

/// Legacy `enter`: the plan becomes active and the workspace current plan.
fn enter(workspace: &Path, plan_id: &str) {
    let db = legacy_db(workspace);
    let body: String = db
        .query_row(
            "SELECT body FROM records WHERE kind='plan' AND id=?1",
            [plan_id],
            |row| row.get(0),
        )
        .unwrap();
    let mut plan: Value = serde_json::from_str(&body).unwrap();
    plan["status"] = json!("active");
    db.execute(
        "UPDATE records SET body=?2 WHERE kind='plan' AND id=?1",
        params![plan_id, plan.to_string()],
    )
    .unwrap();
    db.execute(
        "INSERT INTO current_plan(singleton,id) VALUES(1,?1) \
         ON CONFLICT(singleton) DO UPDATE SET id=excluded.id",
        [plan_id],
    )
    .unwrap();
}

/// Legacy `exit`: only the current-plan row is removed; the record body stays.
fn exit(workspace: &Path) {
    legacy_db(workspace)
        .execute("DELETE FROM current_plan", [])
        .unwrap();
}

fn session_tasks(data: &Path, session: &str) -> Vec<Value> {
    tasks::execute_for_session("TaskList", json!({}), data, session).unwrap()["tasks"]
        .as_array()
        .unwrap()
        .clone()
}

fn import_args<'a>(
    data: &'a str,
    workspace: &'a str,
    selector: &'a str,
    dry_run: bool,
) -> Vec<&'a str> {
    let mut args = vec![
        "--data-dir",
        data,
        "tasks",
        "import-plan",
        "--workspace",
        workspace,
        "--session",
        SESSION,
    ];
    args.extend([selector]);
    if dry_run {
        args.push("--dry-run");
    }
    args
}

#[test]
fn import_plan_creates_tasks_that_the_task_store_can_read() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = workspace(temp.path());
    let plan_id = create_plan(
        &workspace,
        json!([
            {"description":"Write code"},
            {"description":"Review code","dependencies":["step-0"]}
        ]),
    );
    enter(&workspace, &plan_id);
    let source = source_bytes(&workspace);
    let data = temp.path().join("data");
    let plan_selector = format!("--plan={plan_id}");
    let output = run(&import_args(
        data.to_str().unwrap(),
        workspace.to_str().unwrap(),
        &plan_selector,
        false,
    ));
    assert_eq!(output.code, Some(0), "{}", output.stderr);
    let created = report(&output);
    assert_eq!(created["status"], "created", "{created}");
    assert_eq!(created["current"], true, "{created}");
    assert_eq!(created["stored_current"], true, "{created}");
    assert_eq!(created["created_tasks"], 3, "{created}");
    assert_eq!(created["created_edges"], 1, "{created}");
    let root = created["root_task_id"].as_str().expect("root task ID");

    // Read the imported rows back through the normal session task store.
    let rows = session_tasks(&data, SESSION);
    assert_eq!(rows.len(), 3);
    let root_task =
        tasks::execute_for_session("TaskGet", json!({"id":root}), &data, SESSION).unwrap();
    assert_eq!(root_task["status"], "in_progress");
    assert_eq!(root_task["metadata"]["plan"]["legacy_status"], "active");
    assert_eq!(root_task["metadata"]["plan"]["current"], true);
    let step_id = |legacy: &str| {
        created["steps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|step| step["legacy_step_id"] == legacy)
            .unwrap()["task_id"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let first =
        tasks::execute_for_session("TaskGet", json!({"id":step_id("step-0")}), &data, SESSION)
            .unwrap();
    let second =
        tasks::execute_for_session("TaskGet", json!({"id":step_id("step-1")}), &data, SESSION)
            .unwrap();
    assert_eq!(first["blocks"], json!([step_id("step-1")]));
    assert_eq!(second["blockedBy"], json!([step_id("step-0")]));

    // The legacy workspace database bytes are unchanged.
    assert_eq!(
        source_bytes(&workspace),
        source,
        "the legacy source changed"
    );

    // A repeat of the unchanged source is idempotent.
    let repeated = run(&import_args(
        data.to_str().unwrap(),
        workspace.to_str().unwrap(),
        &plan_selector,
        false,
    ));
    assert_eq!(repeated.code, Some(0), "{}", repeated.stderr);
    let repeated = report(&repeated);
    assert_eq!(repeated["status"], "unchanged", "{repeated}");
    assert_eq!(repeated["created_tasks"], 0, "{repeated}");
    assert_eq!(session_tasks(&data, SESSION), rows);
    assert_eq!(source_bytes(&workspace), source);
}

#[test]
fn dry_run_and_invalid_arguments_do_not_create_target_storage() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = workspace(temp.path());
    let plan_id = create_plan(&workspace, json!([{"description":"One"}]));
    let source = source_bytes(&workspace);
    let data = temp.path().join("data");
    let data_arg = data.to_str().unwrap();

    // A dry run reports what would happen and writes nothing.
    let output = run(&import_args(
        data_arg,
        workspace.to_str().unwrap(),
        &format!("--plan={plan_id}"),
        true,
    ));
    assert_eq!(output.code, Some(0), "{}", output.stderr);
    let report = report(&output);
    assert_eq!(report["status"], "created");
    assert_eq!(report["dry_run"], true);
    assert!(report["root_task_id"].is_null());
    assert!(!data.exists(), "a dry run created the data directory");
    assert_eq!(source_bytes(&workspace), source);

    // Exactly one selector is required.
    let missing = run(&[
        "--data-dir",
        data_arg,
        "tasks",
        "import-plan",
        "--workspace",
        workspace.to_str().unwrap(),
        "--session",
        SESSION,
    ]);
    assert_eq!(missing.code, Some(2), "{}", missing.stderr);
    let both = run(&[
        "--data-dir",
        data_arg,
        "tasks",
        "import-plan",
        "--workspace",
        workspace.to_str().unwrap(),
        "--session",
        SESSION,
        &format!("--plan={plan_id}"),
        "--current",
    ]);
    assert_eq!(both.code, Some(2), "{}", both.stderr);

    // Missing/invalid sources and arguments fail without creating storage.
    let empty = temp.path().join("empty");
    std::fs::create_dir_all(&empty).unwrap();
    let missing_source = run(&import_args(
        data_arg,
        empty.to_str().unwrap(),
        &format!("--plan={plan_id}"),
        false,
    ));
    assert_eq!(missing_source.code, Some(1), "{}", missing_source.stderr);
    assert!(
        missing_source.stderr.contains("No legacy plan database"),
        "{}",
        missing_source.stderr
    );
    let bad_session = run(&[
        "--data-dir",
        data_arg,
        "tasks",
        "import-plan",
        "--workspace",
        workspace.to_str().unwrap(),
        "--session",
        "not-a-session",
        &format!("--plan={plan_id}"),
    ]);
    assert_eq!(bad_session.code, Some(1), "{}", bad_session.stderr);
    assert!(
        bad_session.stderr.contains("Invalid target session ID"),
        "{}",
        bad_session.stderr
    );
    let bad_plan = run(&import_args(
        data_arg,
        workspace.to_str().unwrap(),
        "--plan=nope",
        false,
    ));
    assert_eq!(bad_plan.code, Some(1), "{}", bad_plan.stderr);
    let no_current = run(&import_args(
        data_arg,
        workspace.to_str().unwrap(),
        "--current",
        false,
    ));
    assert_eq!(no_current.code, Some(1), "{}", no_current.stderr);
    assert!(
        no_current.stderr.contains("no current plan"),
        "{}",
        no_current.stderr
    );
    assert!(!data.exists(), "a rejected import created target storage");
    assert_eq!(source_bytes(&workspace), source);
}

#[test]
fn current_plan_only_change_is_an_unambiguous_conflict() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = workspace(temp.path());
    let plan_id = create_plan(&workspace, json!([{"description":"One"}]));
    enter(&workspace, &plan_id);
    let data = temp.path().join("data");
    let data_arg = data.to_str().unwrap();
    let plan_selector = format!("--plan={plan_id}");
    let first = run(&import_args(
        data_arg,
        workspace.to_str().unwrap(),
        &plan_selector,
        false,
    ));
    assert_eq!(first.code, Some(0), "{}", first.stderr);
    let root = report(&first)["root_task_id"].as_str().unwrap().to_owned();
    let before = session_tasks(&data, SESSION);

    // The legacy exit path removes only current_plan; the record body stays.
    exit(&workspace);
    let changed_source = source_bytes(&workspace);
    for dry_run in [false, true] {
        let output = run(&import_args(
            data_arg,
            workspace.to_str().unwrap(),
            &plan_selector,
            dry_run,
        ));
        assert_eq!(
            output.code,
            Some(1),
            "a conflict must not exit successfully (dry_run={dry_run}): {}",
            output.stderr
        );
        let report = report(&output);
        assert_eq!(report["status"], "conflict", "{report}");
        assert_eq!(report["current"], false, "{report}");
        assert_eq!(report["stored_current"], true, "{report}");
        assert!(!report["conflicts"].as_array().unwrap().is_empty());
        assert_eq!(session_tasks(&data, SESSION), before);
        assert_eq!(source_bytes(&workspace), changed_source);
    }
    let stored = tasks::execute_for_session("TaskGet", json!({"id":root}), &data, SESSION).unwrap();
    assert_eq!(stored["metadata"]["plan"]["current"], true);
}

#[test]
fn import_uses_the_selected_data_directory_only() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = workspace(temp.path());
    let plan_id = create_plan(&workspace, json!([{"description":"One"}]));
    let source = source_bytes(&workspace);
    let first_data = temp.path().join("first-data");
    let second_data = temp.path().join("second-data");
    let selector = format!("--plan={plan_id}");
    for data in [&first_data, &second_data] {
        let output = run(&import_args(
            data.to_str().unwrap(),
            workspace.to_str().unwrap(),
            &selector,
            false,
        ));
        assert_eq!(output.code, Some(0), "{}", output.stderr);
        assert!(data
            .join("tasks")
            .join(format!("{SESSION}.sqlite3"))
            .is_file());
    }
    assert_eq!(session_tasks(&first_data, SESSION).len(), 2);
    assert_eq!(session_tasks(&second_data, SESSION).len(), 2);
    assert_ne!(
        session_tasks(&first_data, SESSION)[0]["id"],
        session_tasks(&second_data, SESSION)[0]["id"],
        "each data directory must own its own task rows"
    );
    assert_eq!(source_bytes(&workspace), source);
}
