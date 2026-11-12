//! ApplyPatch through the real CLI engine: dispatch, permissions, hooks,
//! persistence and actual file bytes. A local OpenAI-compatible mock returns
//! the tool calls; every assertion reads the real database and workspace.
#[path = "support/tool_loading.rs"]
mod tool_loading;

use axum::{extract::State, routing::post, Json, Router};
use deepseek_code_core::{
    db::message_repo,
    runtime::{RuntimePaths, SessionStore},
};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

/// One scripted model: the nth request returns `responses[n]` (a tool call) and
/// the final request returns plain text.
struct Fixture {
    responses: Vec<(&'static str, Value)>,
    requests: Mutex<Vec<Value>>,
    calls: AtomicUsize,
}

/// A distinct provider call id per scripted response. Reused ids are a separate
/// occurrence-identity concern; a local fixture must not accidentally exercise
/// (or mask) that behavior.
fn call_id(index: usize) -> String {
    format!("apply-fixture-call-{index}")
}

fn stream(tool: Option<(&str, Value, &str)>, text: &str) -> String {
    let is_tool = tool.is_some();
    let delta = match tool {
        Some((name, input, id)) => json!({"tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":name,"arguments":input.to_string()}}]}),
        None => json!({"content": text}),
    };
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":if is_tool{"tool_calls"}else{"stop"}}]})
    )
}

async fn respond(State(state): State<Arc<Fixture>>, Json(body): Json<Value>) -> impl axum::response::IntoResponse {
    let index = state.calls.fetch_add(1, Ordering::SeqCst);
    state.requests.lock().unwrap().push(body);
    let id = call_id(index);
    let (tool, text) = match state.responses.get(index).cloned() {
        Some((name, input)) => (Some((name, input, id.as_str())), ""),
        None => (None, "ApplyPatch fixture completed"),
    };
    ([("content-type", "text/event-stream")], stream(tool, text))
}

struct Env {
    _temp: tempfile::TempDir,
    workspace: PathBuf,
    data: PathBuf,
    endpoint: String,
    server: ServerGuard,
    fixture: Arc<Fixture>,
}

/// Aborts the local mock when the test ends, including on panic.
struct ServerGuard(tokio::task::JoinHandle<()>);

impl Drop for ServerGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn start(responses: Vec<(&'static str, Value)>) -> Env {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    let data = temp.path().join("data");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&data).unwrap();
    let fixture = Arc::new(Fixture {
        responses,
        requests: Mutex::new(Vec::new()),
        calls: AtomicUsize::new(0),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/chat/completions", post(respond))
        .with_state(fixture.clone());
    let server = ServerGuard(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap()
    }));
    Env {
        _temp: temp,
        workspace,
        data,
        endpoint,
        server,
        fixture,
    }
}

/// Runs the real CLI. The wait is asynchronous: the default current-thread test
/// runtime must stay free to serve the local mock, and a synchronous
/// `std::process::Command::output` would deadlock both sides. The bound also
/// guarantees cleanup if the child never exits.
async fn run(env: &Env, prompt: &str, extra: &[&str]) -> std::process::Output {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_dcl"));
    command
        .current_dir(&env.workspace)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .args([
            "--data-dir",
            env.data.to_str().unwrap(),
            "--base-url",
            &env.endpoint,
            "--api-format",
            "openai",
            "--api-key-env",
            "APPLY_PATCH_FIXTURE_KEY",
            "--model",
            "fixture",
            "--print",
            "--output-format",
            "stream-json",
            "--permission-mode",
        ])
        .args(extra)
        .args(["--", prompt])
        .env("APPLY_PATCH_FIXTURE_KEY", "fixture-only")
        .env("NO_PROXY", "127.0.0.1");
    for (key, _) in std::env::vars() {
        if key.starts_with("DEEPSEEK_CODE") {
            command.env_remove(key);
        }
    }
    tokio::time::timeout(Duration::from_secs(60), command.output())
        .await
        .expect("the CLI did not exit within 60s; the fixture child was killed")
        .expect("spawning the CLI")
}

/// Every persisted tool call, in execution order, with its session.
fn persisted_tool_calls(data: &Path, session: &str) -> Vec<message_repo::ToolCallRow> {
    let paths = RuntimePaths::new(data.to_path_buf()).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let messages = store
        .db
        .with_conn(|conn| message_repo::get_messages_by_conversation(conn, session))
        .unwrap()
        .unwrap();
    let mut calls = Vec::new();
    for message in &messages {
        calls.extend(
            store
                .db
                .with_conn(|conn| message_repo::get_tool_calls_for_message(conn, &message.id))
                .unwrap()
                .unwrap(),
        );
    }
    calls
}

/// Tool failures are delivered to the model as `Error: <json>`.
fn tool_json(output: &Value) -> Value {
    let text = output.as_str().unwrap_or_default();
    let text = text.strip_prefix("Error: ").unwrap_or(text);
    serde_json::from_str(text).unwrap_or_else(|error| panic!("tool output is not JSON ({error}): {text}"))
}

fn stdout_events(output: &std::process::Output) -> Vec<Value> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.starts_with('{'))
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// The tool message content in a recorded OpenAI request.
fn last_tool_output(body: &Value) -> Option<String> {
    body["messages"]
        .as_array()?
        .iter()
        .filter(|message| message["role"] == "tool")
        .next_back()
        .and_then(|message| message["content"].as_str().map(str::to_owned))
}

fn session_id(output: &std::process::Output, data: &Path) -> String {
    // The CLI's result event carries the session id; fall back to the only
    // session recorded in the isolated data directory.
    if let Some(id) = stdout_events(output)
        .iter()
        .find_map(|event| event["session_id"].as_str().map(str::to_owned))
    {
        return id;
    }
    let paths = RuntimePaths::new(data.to_path_buf()).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    store
        .db
        .with_conn(|conn| {
            conn.query_row("SELECT id FROM sessions LIMIT 1", [], |row| row.get::<_, String>(0))
                .ok()
        })
        .unwrap()
        .expect("a session row")
}

const MIXED_PATCH: &str = "\
*** Begin Patch
*** Add File: sub/added.txt
+new line
*** Update File: edit.txt
@@
 one
-two
+TWO
*** Delete File: del.txt
*** Update File: move.txt
*** Move to: moved.txt
@@
-old
+NEW
*** End Patch";

fn write_mixed_tree(workspace: &Path) {
    std::fs::create_dir_all(workspace.join("sub")).unwrap();
    std::fs::write(workspace.join("edit.txt"), "one\ntwo\n").unwrap();
    std::fs::write(workspace.join("del.txt"), "gone\n").unwrap();
    std::fs::write(workspace.join("move.txt"), "old\n").unwrap();
}

#[tokio::test]
async fn mixed_patch_applies_through_the_engine_and_persists_one_call() {
    let env = start(vec![("ApplyPatch", json!({"patch": MIXED_PATCH}))]).await;
    write_mixed_tree(&env.workspace);
    tool_loading::enable(&env.data, &["ApplyPatch"]);
    let output = run(&env, "apply the patch", &["bypass"]).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(env.workspace.join("sub/added.txt")).unwrap(),
        "new line\n"
    );
    assert_eq!(
        std::fs::read_to_string(env.workspace.join("edit.txt")).unwrap(),
        "one\nTWO\n"
    );
    assert!(!env.workspace.join("del.txt").exists());
    assert_eq!(
        std::fs::read_to_string(env.workspace.join("moved.txt")).unwrap(),
        "NEW\n"
    );
    assert!(!env.workspace.join("move.txt").exists());

    let events = stdout_events(&output);
    let result = events
        .iter()
        .find(|event| event["type"] == "tool_result" && event["name"] == "ApplyPatch")
        .expect("tool result event");
    assert_eq!(result["is_error"], false, "{result}");
    let value = tool_json(&result["output"]);
    assert_eq!(value["success"], true, "{value}");
    assert_eq!(value["applied"].as_array().unwrap().len(), 4);
    // A multi-file result has no single top-level diff.
    assert!(value["file_change"].is_null());

    // The result the model receives in the NEXT request carries the paths,
    // counts and unresolved state but not the bulk per-file diff rows.
    let requests = env.fixture.requests.lock().unwrap();
    let model_output = last_tool_output(&requests[1]).expect("tool message");
    let model_value = tool_json(&Value::String(model_output));
    assert_eq!(model_value["applied"].as_array().unwrap().len(), 4);
    assert!(model_value["file_change"].is_null());
    for change in model_value["changes"].as_array().unwrap() {
        assert!(change.get("file_change").is_none(), "{change}");
    }
    drop(requests);

    // The persisted record (what the UI/rewind reads) keeps the diff metadata.
    let id = session_id(&output, &env.data);
    let calls = persisted_tool_calls(&env.data, &id);
    let applies: Vec<_> = calls.iter().filter(|call| call.name == "ApplyPatch").collect();
    assert_eq!(applies.len(), 1, "one persisted ApplyPatch call");
    assert!(!applies[0].is_error);
    let stored: Value =
        serde_json::from_str(applies[0].output.as_deref().expect("stored output")).unwrap();
    assert_eq!(stored["success"], true);
    assert_eq!(stored["applied"].as_array().unwrap().len(), 4);
    let updated = stored["changes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|change| change["raw_path"] == "edit.txt")
        .expect("stored update");
    assert_eq!(updated["file_change"]["verified"], true);
    assert_eq!(updated["file_change"]["added"], 1);
}

#[tokio::test]
async fn a_mismatch_in_a_later_file_changes_nothing_and_reports_an_error() {
    let env = start(vec![(
        "ApplyPatch",
        json!({"patch": "\
*** Begin Patch
*** Update File: a.txt
@@
-one
+ONE
*** Update File: b.txt
@@
-absent
+X
*** End Patch"}),
    )])
    .await;
    std::fs::write(env.workspace.join("a.txt"), "one\n").unwrap();
    std::fs::write(env.workspace.join("b.txt"), "two\n").unwrap();
    tool_loading::enable(&env.data, &["ApplyPatch"]);
    let output = run(&env, "apply the patch", &["bypass"]).await;
    assert_eq!(
        std::fs::read_to_string(env.workspace.join("a.txt")).unwrap(),
        "one\n"
    );
    assert_eq!(
        std::fs::read_to_string(env.workspace.join("b.txt")).unwrap(),
        "two\n"
    );
    let events = stdout_events(&output);
    let result = events
        .iter()
        .find(|event| event["type"] == "tool_result" && event["name"] == "ApplyPatch")
        .expect("tool result event");
    assert_eq!(result["is_error"], true, "{result}");
    let value = tool_json(&result["output"]);
    assert_eq!(value["success"], false);
    assert_eq!(value["error"]["kind"], "path");
    assert!(value["error"]["message"].as_str().unwrap().contains("b.txt"));
    assert!(value["applied"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn traversal_outside_the_workspace_is_refused_before_any_change() {
    let env = start(vec![(
        "ApplyPatch",
        json!({"patch": "\
*** Begin Patch
*** Update File: inside.txt
@@
-one
+ONE
*** Delete File: ../outside.txt
*** End Patch"}),
    )])
    .await;
    let outside = env._temp.path().join("outside.txt");
    std::fs::write(&outside, "secret\n").unwrap();
    std::fs::write(env.workspace.join("inside.txt"), "one\n").unwrap();
    tool_loading::enable(&env.data, &["ApplyPatch"]);
    let output = run(&env, "apply the patch", &["bypass"]).await;
    assert_eq!(
        std::fs::read_to_string(env.workspace.join("inside.txt")).unwrap(),
        "one\n"
    );
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "secret\n");
    let events = stdout_events(&output);
    let result = events
        .iter()
        .find(|event| event["type"] == "tool_result" && event["name"] == "ApplyPatch")
        .expect("tool result event");
    assert_eq!(result["is_error"], true, "{result}");
}

#[tokio::test]
async fn plan_mode_denies_the_patch_without_touching_files() {
    let env = start(vec![("ApplyPatch", json!({"patch": MIXED_PATCH}))]).await;
    write_mixed_tree(&env.workspace);
    tool_loading::enable(&env.data, &["ApplyPatch"]);
    let output = run(&env, "apply the patch", &["plan"]).await;
    assert_eq!(
        std::fs::read_to_string(env.workspace.join("edit.txt")).unwrap(),
        "one\ntwo\n"
    );
    assert!(env.workspace.join("del.txt").exists());
    assert!(!env.workspace.join("sub/added.txt").exists());
    let events = stdout_events(&output);
    let result = events
        .iter()
        .find(|event| event["type"] == "tool_result" && event["name"] == "ApplyPatch")
        .expect("tool result event");
    assert_eq!(result["is_error"], true, "{result}");
    assert!(result["output"]
        .as_str()
        .unwrap()
        .to_lowercase()
        .contains("permission denied"));
}

fn settings_with_hook(data: &Path, matcher: &str, command: &str) {
    std::fs::write(
        data.join("settings.json"),
        json!({"hooks":{"PreToolUse":[{"matcher":matcher,"hooks":[{"type":"command","command":command}]}]}})
            .to_string(),
    )
    .unwrap();
}

#[tokio::test]
async fn a_rejecting_hook_leaves_every_target_unchanged() {
    let env = start(vec![("ApplyPatch", json!({"patch": MIXED_PATCH}))]).await;
    write_mixed_tree(&env.workspace);
    tool_loading::enable(&env.data, &["ApplyPatch"]);
    settings_with_hook(&env.data, "ApplyPatch", "exit 2");
    let output = run(&env, "apply the patch", &["bypass"]).await;
    assert_eq!(
        std::fs::read_to_string(env.workspace.join("edit.txt")).unwrap(),
        "one\ntwo\n"
    );
    assert!(env.workspace.join("del.txt").exists());
    assert!(env.workspace.join("move.txt").exists());
    assert!(!env.workspace.join("sub/added.txt").exists());
    let events = stdout_events(&output);
    let result = events
        .iter()
        .find(|event| event["type"] == "tool_result" && event["name"] == "ApplyPatch")
        .expect("tool result event");
    assert_eq!(result["is_error"], true, "{result}");
    assert!(result["output"]
        .as_str()
        .unwrap()
        .contains("PreToolUse hook blocked tool"));
}

#[tokio::test]
async fn a_hook_rewritten_patch_still_obeys_path_scope() {
    let env = start(vec![("ApplyPatch", json!({"patch": MIXED_PATCH}))]).await;
    write_mixed_tree(&env.workspace);
    let outside = env._temp.path().join("outside.txt");
    std::fs::write(&outside, "secret\n").unwrap();
    tool_loading::enable(&env.data, &["ApplyPatch"]);
    // The hook replaces the patch with one that targets a file outside the
    // workspace. Authorization must run on the rewritten input.
    let rewritten = format!(
        "*** Begin Patch\n*** Delete File: {}\n*** End Patch",
        outside.display()
    );
    let response = json!({"hookSpecificOutput": {
        "hookEventName": "PreToolUse",
        "permissionDecision": "allow",
        "updatedInput": {"patch": rewritten}
    }})
    .to_string();
    let script = env.workspace.join("rewrite.sh");
    std::fs::write(&script, format!("#!/bin/sh\ncat <<'JSON'\n{response}\nJSON\n")).unwrap();
    settings_with_hook(
        &env.data,
        "ApplyPatch",
        &format!("/bin/sh {}", script.display()),
    );
    let output = run(&env, "apply the patch", &["bypass"]).await;
    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "secret\n");
    assert_eq!(
        std::fs::read_to_string(env.workspace.join("edit.txt")).unwrap(),
        "one\ntwo\n"
    );
    assert!(env.workspace.join("del.txt").exists());
    let events = stdout_events(&output);
    let result = events
        .iter()
        .find(|event| event["type"] == "tool_result" && event["name"] == "ApplyPatch")
        .expect("tool result event");
    assert_eq!(result["is_error"], true, "{result}");
    let text = result["output"].as_str().unwrap().to_lowercase();
    assert!(
        text.contains("outside the trusted") || text.contains("outside the workspace"),
        "{text}"
    );
}

#[tokio::test]
async fn tool_search_discovers_apply_patch_once_and_it_runs_after_loading() {
    let env = start(vec![
        ("ToolSearch", json!({"query": "ApplyPatch"})),
        ("ApplyPatch", json!({"patch": "*** Begin Patch\n*** Update File: a.txt\n@@\n-one\n+ONE\n*** End Patch"})),
    ])
    .await;
    std::fs::write(env.workspace.join("a.txt"), "one\n").unwrap();
    // Default loading: ApplyPatch is on demand and not resident.
    let output = run(&env, "change a.txt", &["bypass"]).await;
    assert_eq!(
        std::fs::read_to_string(env.workspace.join("a.txt")).unwrap(),
        "ONE\n"
    );
    let requests = env.fixture.requests.lock().unwrap();
    assert_eq!(requests.len(), 3, "search, apply, final text");
    // The first request does not advertise ApplyPatch; the search result and
    // the following request carry its definition exactly once.
    let names = |request: &Value| -> Vec<String> {
        request["tools"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|tool| tool["function"]["name"].as_str().map(str::to_owned))
            .collect()
    };
    assert!(!names(&requests[0]).iter().any(|name| name == "ApplyPatch"));
    let after = names(&requests[1]);
    assert_eq!(
        after.iter().filter(|name| *name == "ApplyPatch").count(),
        1,
        "{after:?}"
    );
    assert!(names(&requests[2]).iter().any(|name| name == "ApplyPatch"));
    let events = stdout_events(&output);
    let search = events
        .iter()
        .find(|event| event["type"] == "tool_result" && event["name"] == "ToolSearch")
        .expect("search result");
    let value = tool_json(&search["output"]);
    assert_eq!(value["success"], true, "{value}");
    assert_eq!(value["loaded_tools"], json!(["ApplyPatch"]));
}

#[tokio::test]
async fn an_unchanged_patch_reports_no_change_and_prior_edit_tools_still_work() {
    let env = start(vec![
        ("ApplyPatch", json!({"patch": "*** Begin Patch\n*** Update File: same.txt\n@@\n-same\n+same\n*** End Patch"})),
        ("Read", json!({"file_path": "same.txt"})),
        ("Edit", json!({"file_path": "same.txt", "old_string": "same", "new_string": "edited"})),
    ])
    .await;
    std::fs::write(env.workspace.join("same.txt"), "same\n").unwrap();
    tool_loading::enable(&env.data, &["ApplyPatch"]);
    let output = run(&env, "try both", &["bypass"]).await;
    assert_eq!(
        std::fs::read_to_string(env.workspace.join("same.txt")).unwrap(),
        "edited\n"
    );
    let events = stdout_events(&output);
    let apply = events
        .iter()
        .find(|event| event["type"] == "tool_result" && event["name"] == "ApplyPatch")
        .expect("apply result");
    assert_eq!(apply["is_error"], false, "{apply}");
    let value = tool_json(&apply["output"]);
    assert!(value["applied"].as_array().unwrap().is_empty());
    assert_eq!(value["unchanged"].as_array().unwrap().len(), 1);
    assert!(value["changes"].as_array().unwrap().is_empty());
    let edit = events
        .iter()
        .find(|event| event["type"] == "tool_result" && event["name"] == "Edit")
        .expect("edit result");
    assert_eq!(edit["is_error"], false, "{edit}");
}

#[tokio::test]
async fn a_move_destination_outside_the_workspace_is_refused_by_the_engine_path() {
    let env = start(vec![(
        "ApplyPatch",
        json!({"patch": "*** Begin Patch\n*** Update File: move.txt\n*** Move to: ../escaped.txt\n*** End Patch"}),
    )])
    .await;
    std::fs::write(env.workspace.join("move.txt"), "old\n").unwrap();
    tool_loading::enable(&env.data, &["ApplyPatch"]);
    let output = run(&env, "move it", &["bypass"]).await;
    assert!(env.workspace.join("move.txt").exists());
    assert!(!env._temp.path().join("escaped.txt").exists());
    let events = stdout_events(&output);
    let result = events
        .iter()
        .find(|event| event["type"] == "tool_result" && event["name"] == "ApplyPatch")
        .expect("tool result event");
    assert_eq!(result["is_error"], true, "{result}");
}

/// Creates a Code session directly (like a first CLI turn would) and persists
/// the session's explicit permission rules before `--resume` reads them.
fn create_resumed_session(env: &Env, rules: Value) -> String {
    let paths = RuntimePaths::new(env.data.clone()).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let id = store.create(&env.workspace, "fixture", "cli-runtime").unwrap();
    if rules.as_object().is_some_and(|rules| !rules.is_empty()) {
        let key = format!("cli.permissions.{id}");
        let value = rules.to_string();
        store
            .db
            .with_conn(|conn| {
                conn.execute(
                    "INSERT OR REPLACE INTO peripheral_settings(key,value) VALUES(?1,?2)",
                    [key, value],
                )
            })
            .unwrap()
            .unwrap();
    }
    id
}

async fn run_resumed(env: &Env, session: &str, prompt: &str, mode: &str) -> std::process::Output {
    run(
        env,
        prompt,
        &[mode, "--resume", session, "--no-interactive"],
    )
    .await
}

fn tool_result<'a>(events: &'a [Value], name: &str) -> &'a Value {
    events
        .iter()
        .find(|event| event["type"] == "tool_result" && event["name"] == name)
        .unwrap_or_else(|| panic!("no {name} tool result in {events:?}"))
}

#[tokio::test]
async fn accept_edits_persisted_confirmation_blocks_apply_patch_and_edit_alike() {
    for (case, responses, rule_tool) in [
        (
            "ApplyPatch",
            vec![(
                "ApplyPatch",
                json!({"patch": "*** Begin Patch\n*** Update File: secret.txt\n@@\n-secret\n+NEW\n*** End Patch"}),
            )],
            "ApplyPatch",
        ),
        (
            "Edit",
            vec![
                ("Read", json!({"file_path": "secret.txt"})),
                (
                    "Edit",
                    json!({"file_path": "secret.txt", "old_string": "secret", "new_string": "NEW"}),
                ),
            ],
            "Edit",
        ),
    ] {
        let env = start(responses).await;
        std::fs::write(env.workspace.join("secret.txt"), "secret\n").unwrap();
        tool_loading::enable(&env.data, &["ApplyPatch"]);
        let session = create_resumed_session(&env, json!({"Edit(secret.txt)": "Confirm"}));
        let output = run_resumed(&env, &session, "perform the file operation", "accept-edits").await;
        assert_eq!(
            std::fs::read_to_string(env.workspace.join("secret.txt")).unwrap(),
            "secret\n",
            "{case} must not silently turn the explicit ask into an allow"
        );
        let events = stdout_events(&output);
        let result = tool_result(&events, rule_tool);
        assert_eq!(result["is_error"], true, "{case}: {result}");
        let text = result["output"].as_str().unwrap().to_lowercase();
        assert!(
            text.contains("permission denied")
                || text.contains("not granted")
                || text.contains("requires confirmation"),
            "{case}: {text}"
        );
    }
}

#[tokio::test]
async fn accept_edits_without_a_path_rule_still_applies_apply_patch() {
    let env = start(vec![(
        "ApplyPatch",
        json!({"patch": "*** Begin Patch\n*** Update File: allowed.txt\n@@\n-old\n+NEW\n*** End Patch"}),
    )])
    .await;
    std::fs::write(env.workspace.join("allowed.txt"), "old\n").unwrap();
    tool_loading::enable(&env.data, &["ApplyPatch"]);
    let session = create_resumed_session(&env, json!({}));
    let output = run_resumed(&env, &session, "apply the patch", "accept-edits").await;
    assert_eq!(
        std::fs::read_to_string(env.workspace.join("allowed.txt")).unwrap(),
        "NEW\n"
    );
    let events = stdout_events(&output);
    let result = tool_result(&events, "ApplyPatch");
    assert_eq!(result["is_error"], false, "{result}");
}

#[tokio::test]
async fn a_move_destination_confirmation_blocks_the_whole_move_in_the_cli() {
    let env = start(vec![(
        "ApplyPatch",
        json!({"patch": "*** Begin Patch\n*** Update File: move.txt\n*** Move to: moved.txt\n*** End Patch"}),
    )])
    .await;
    std::fs::write(env.workspace.join("move.txt"), "old\n").unwrap();
    tool_loading::enable(&env.data, &["ApplyPatch"]);
    let session = create_resumed_session(&env, json!({"Edit(moved.txt)": "Confirm"}));
    let output = run_resumed(&env, &session, "move it", "accept-edits").await;
    assert_eq!(
        std::fs::read_to_string(env.workspace.join("move.txt")).unwrap(),
        "old\n"
    );
    assert!(!env.workspace.join("moved.txt").exists());
    let events = stdout_events(&output);
    let result = tool_result(&events, "ApplyPatch");
    assert_eq!(result["is_error"], true, "{result}");
}

#[tokio::test]
async fn a_hook_rewritten_target_cannot_inherit_approval_or_bypass_a_deny() {
    for action in ["Confirm", "Deny"] {
        let env = start(vec![(
            "ApplyPatch",
            json!({"patch": "*** Begin Patch\n*** Update File: a.txt\n@@\n-one\n+ONE\n*** End Patch"}),
        )])
        .await;
        std::fs::write(env.workspace.join("a.txt"), "one\n").unwrap();
        std::fs::write(env.workspace.join("secret.txt"), "secret\n").unwrap();
        tool_loading::enable(&env.data, &["ApplyPatch"]);
        let session = create_resumed_session(&env, json!({"Edit(secret.txt)": action}));
        let response = json!({"hookSpecificOutput":{
            "hookEventName":"PreToolUse",
            "permissionDecision":"allow",
            "updatedInput":{"patch":"*** Begin Patch\n*** Update File: secret.txt\n@@\n-secret\n+LEAK\n*** End Patch"}
        }})
        .to_string();
        let script = env.workspace.join("rewrite.sh");
        std::fs::write(&script, format!("#!/bin/sh\ncat <<'JSON'\n{response}\nJSON\n")).unwrap();
        settings_with_hook(
            &env.data,
            "ApplyPatch",
            &format!("/bin/sh {}", script.display()),
        );
        let output = run_resumed(&env, &session, "apply the patch", "accept-edits").await;
        assert_eq!(
            std::fs::read_to_string(env.workspace.join("a.txt")).unwrap(),
            "one\n",
            "{action}: the hook replaced the patch, so the original target must be untouched"
        );
        assert_eq!(
            std::fs::read_to_string(env.workspace.join("secret.txt")).unwrap(),
            "secret\n",
            "{action}: a hook allow cannot transfer approval to the rewritten target"
        );
        let events = stdout_events(&output);
        let result = tool_result(&events, "ApplyPatch");
        assert_eq!(result["is_error"], true, "{action}: {result}");
    }
}
