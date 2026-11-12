//! ApplyPatch across the real SDK control protocol: the standalone CLI process,
//! the actual `control_request`/`control_response` frames and a local model
//! fixture. These are protocol tests, not engine-mailbox simulations.
use super::*;
use deepseek_code_core::{
    db::message_repo,
    runtime::{RuntimePaths, SessionStore},
};

fn data(root: &Path) -> std::path::PathBuf {
    root.join("data")
}

/// Persist the same session rule the round-09/11 counterexample used, then let
/// the CLI resume that exact Code session. The rule is read back by the normal
/// `permission_commands::load` path on every turn.
fn asked_session(root: &Path, confirm: &[&str]) -> String {
    let paths = RuntimePaths::new(data(root)).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let id = uuid::Uuid::new_v4().to_string();
    store
        .create_with_id(root, "fixture-model", "cli-runtime", &id)
        .unwrap();
    let rules = Value::Object(
        confirm
            .iter()
            .map(|path| (format!("Edit({path})"), json!("Confirm")))
            .collect(),
    );
    let key = format!("cli.permissions.{id}");
    store
        .db
        .with_conn(|conn| {
            conn.execute(
                "INSERT OR REPLACE INTO peripheral_settings(key,value) VALUES(?1,?2)",
                [key.clone(), rules.to_string()],
            )
        })
        .unwrap()
        .unwrap();
    let stored: String = store
        .db
        .with_conn(|conn| {
            conn.query_row(
                "SELECT value FROM peripheral_settings WHERE key=?1",
                [key],
                |row| row.get(0),
            )
        })
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&stored).unwrap(),
        rules,
        "the scenario must persist the same explicit rules it resumes"
    );
    id
}

/// Optional settings such as hooks, without a second permission source.
fn settings(root: &Path, value: Value) {
    std::fs::create_dir_all(data(root)).unwrap();
    std::fs::write(data(root).join("settings.json"), value.to_string()).unwrap();
}

/// ApplyPatch is resident, so a missing-tool rejection cannot be mistaken for
/// the permission path under test.
fn prepare(root: &Path) {
    std::fs::write(root.join("a.txt"), "one\n").unwrap();
    std::fs::write(root.join("secret.txt"), "secret\n").unwrap();
    std::fs::create_dir_all(data(root)).unwrap();
    super::tool_loading::enable(&data(root), &["ApplyPatch"]);
}

fn unchanged(root: &Path) {
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "one\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("secret.txt")).unwrap(),
        "secret\n"
    );
}

/// The next value the run emitted, as long as that value is a frame.
async fn next_line(client: &mut Client) -> Value {
    let line = client
        .output
        .next_line()
        .await
        .unwrap()
        .expect("CLI ended before the expected SDK frame");
    serde_json::from_str(&line).unwrap_or_else(|_| panic!("Non-JSON stdout: {line}"))
}

/// Waits for the first `control_request` without answering it.
async fn permission_request(client: &mut Client) -> Value {
    permission_request_into(client, &mut Vec::new()).await
}

/// The same, keeping every frame it passes so later assertions can inspect the
/// tool result that followed an earlier prompt.
async fn permission_request_into(client: &mut Client, frames: &mut Vec<Value>) -> Value {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let frame = next_line(client).await;
            assert!(
                frame["type"] != "result",
                "the turn ended before requesting permission: {frame}"
            );
            let request = frame["type"] == "control_request";
            frames.push(frame);
            if request {
                return frames.last().unwrap().clone();
            }
        }
    })
    .await
    .expect("no SDK permission request")
}

async fn frames_until_result(client: &mut Client) -> Vec<Value> {
    let mut frames = Vec::new();
    frames_until_result_into(client, &mut frames).await;
    frames
}

async fn frames_until_result_into(client: &mut Client, frames: &mut Vec<Value>) {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let frame = next_line(client).await;
            let last = frame["type"] == "result";
            frames.push(frame);
            if last {
                return;
            }
        }
    })
    .await
    .expect("CLI did not emit a result while stdin stayed open")
}

fn answer(request: &Value, behavior: &str) -> Value {
    let mut response = json!({"behavior":behavior});
    if behavior == "allow" {
        response["updatedInput"] = request["request"]["input"].clone();
    }
    json!({"type":"control_response","response":{"subtype":"success","request_id":request["request_id"],"response":response}})
}

fn tool_results<'a>(frames: &'a [Value], id: &str) -> Vec<&'a Value> {
    frames
        .iter()
        .filter(|frame| frame["type"] == "user")
        .filter_map(|frame| frame["message"]["content"].as_array())
        .flatten()
        .filter(|block| block["type"] == "tool_result" && block["tool_use_id"] == id)
        .collect()
}

pub(super) fn persisted_tool_calls(root: &Path, session: &str) -> Vec<message_repo::ToolCallRow> {
    let paths = RuntimePaths::new(data(root)).unwrap();
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

/// The prompt must name the effective occurrence, never the provider's
/// reusable raw id, and must carry its own request id. Returns the occurrence.
fn one_permission_request(request: &Value) -> String {
    assert_eq!(request["request"]["subtype"], "can_use_tool", "{request}");
    assert_eq!(request["request"]["tool_name"], "ApplyPatch", "{request}");
    let occurrence = request["request"]["tool_use_id"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(
        occurrence.starts_with("inv-"),
        "an occurrence id is expected: {request}"
    );
    assert_ne!(
        occurrence, APPLY_PATCH_PROVIDER_ID,
        "the prompt must not expose the provider's reusable raw id"
    );
    assert!(
        request["request_id"]
            .as_str()
            .is_some_and(|id| !id.is_empty()),
        "{request}"
    );
    occurrence
}

#[tokio::test]
async fn sdk_allow_runs_one_apply_patch_after_the_effective_patch_is_reviewed() {
    let server = Server::start(false, Mode::ApplyPatchMixed).await;
    let root = tempfile::tempdir().unwrap();
    prepare(root.path());
    let session = asked_session(root.path(), &["secret.txt"]);
    let mut client = Client::start(root.path(), &server, &["--resume", session.as_str()]);
    client.send(user("apply the patch")).await;

    let request = permission_request(&mut client).await;
    let occurrence = one_permission_request(&request);
    assert_eq!(request["request"]["input"]["patch"], APPLY_PATCH_MIXED);
    // Nothing is written before the host answers the actual request.
    unchanged(root.path());

    client.send(answer(&request, "allow")).await;
    let frames = frames_until_result(&mut client).await;
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame["type"] == "control_request")
            .count(),
        0,
        "one ApplyPatch call must not prompt twice"
    );
    let result = frames.last().unwrap();
    assert_eq!(result["is_error"], false, "{result}");
    assert_eq!(
        std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
        "ONE\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("secret.txt")).unwrap(),
        "LEAK\n"
    );
    // The result and the persisted row carry the same occurrence the prompt did.
    assert_eq!(tool_results(&frames, &occurrence).len(), 1);

    let session = result["session_id"].as_str().unwrap();
    let calls = persisted_tool_calls(root.path(), session);
    let applies: Vec<_> = calls
        .iter()
        .filter(|call| call.name == "ApplyPatch")
        .collect();
    assert_eq!(applies.len(), 1, "one persisted ApplyPatch record");
    assert!(
        applies[0].id.ends_with(&format!(":{occurrence}")),
        "persisted row {} must belong to {occurrence}",
        applies[0].id
    );
    assert!(!applies[0].is_error, "{:?}", applies[0].output);
    let stored: Value =
        serde_json::from_str(applies[0].output.as_deref().expect("stored output")).unwrap();
    assert_eq!(stored["success"], true, "{stored}");
    assert_eq!(stored["applied"].as_array().unwrap().len(), 2);
    // Provider-facing protocol still pairs the raw provider id on both the
    // assistant call and the tool result message.
    {
        let requests = server.fixture.requests.lock().unwrap();
        let body = requests.last().expect("the follow-up provider request");
        let messages = body["messages"].as_array().unwrap();
        let assistant_id = messages
            .iter()
            .filter(|message| message["role"] == "assistant")
            .filter_map(|message| message["tool_calls"].as_array())
            .flatten()
            .filter_map(|call| call["id"].as_str())
            .next_back()
            .expect("the assistant tool call");
        let result_id = messages
            .iter()
            .filter(|message| message["role"] == "tool")
            .filter_map(|message| message["tool_call_id"].as_str())
            .next_back()
            .expect("the tool result message");
        assert_eq!(assistant_id, APPLY_PATCH_PROVIDER_ID);
        assert_eq!(result_id, APPLY_PATCH_PROVIDER_ID);
        assert_ne!(occurrence, APPLY_PATCH_PROVIDER_ID);
    }
    assert!(client.finish().await.0.success());
}

#[tokio::test]
async fn sdk_deny_leaves_every_target_of_the_mixed_patch_unchanged() {
    let server = Server::start(false, Mode::ApplyPatchMixed).await;
    let root = tempfile::tempdir().unwrap();
    prepare(root.path());
    let session = asked_session(root.path(), &["secret.txt"]);
    let mut client = Client::start(root.path(), &server, &["--resume", session.as_str()]);
    client.send(user("apply the patch")).await;

    let request = permission_request(&mut client).await;
    let occurrence = one_permission_request(&request);
    unchanged(root.path());
    client.send(answer(&request, "deny")).await;
    let frames = frames_until_result(&mut client).await;
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame["type"] == "control_request")
            .count(),
        0,
        "a denial must not be asked again"
    );
    let result = frames.last().unwrap();
    assert_eq!(result["is_error"], false, "{result}");
    let denials = result["permission_denials"].as_array().unwrap();
    assert_eq!(denials.len(), 1, "{result}");
    assert_eq!(denials[0]["tool_name"], "ApplyPatch", "{result}");
    assert_eq!(denials[0]["tool_input"]["patch"], APPLY_PATCH_MIXED);
    assert_eq!(denials[0]["tool_use_id"], occurrence, "{result}");

    let results = tool_results(&frames, &occurrence);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["is_error"], true, "{results:?}");
    let text = results[0].to_string().to_lowercase();
    assert!(
        text.contains("denied") || text.contains("approval failed"),
        "{text}"
    );
    unchanged(root.path());

    let session = result["session_id"].as_str().unwrap();
    let calls = persisted_tool_calls(root.path(), session);
    let applies: Vec<_> = calls
        .iter()
        .filter(|call| call.name == "ApplyPatch")
        .collect();
    assert!(
        applies.iter().all(|call| call.is_error),
        "a denied call must never look successful: {applies:?}"
    );
    assert!(client.finish().await.0.success());
}

#[tokio::test]
async fn sdk_input_closed_while_the_patch_waits_for_approval_never_writes() {
    let server = Server::start(false, Mode::ApplyPatchMixed).await;
    let root = tempfile::tempdir().unwrap();
    prepare(root.path());
    let session = asked_session(root.path(), &["secret.txt"]);
    let mut client = Client::start(root.path(), &server, &["--resume", session.as_str()]);
    client.send(user("apply the patch")).await;

    let request = permission_request(&mut client).await;
    let occurrence = one_permission_request(&request);
    unchanged(root.path());

    // Closing the input while the real request is pending must end the process
    // at the existing deadline without applying anything.
    let (status, results) = client.finish().await;
    assert!(
        !status.success(),
        "cancellation must stay distinct from normal completion"
    );
    assert_eq!(results.len(), 1, "{results:?}");
    assert_eq!(results[0]["is_error"], true);
    unchanged(root.path());
    let session = results[0]["session_id"].as_str().unwrap();
    let calls = persisted_tool_calls(root.path(), session);
    assert!(
        calls
            .iter()
            .filter(|call| call.name == "ApplyPatch")
            .all(|call| call.is_error),
        "the cancelled call must not persist as success: {calls:?}"
    );
    super::approval_cancellation::assert_cancelled_call(
        root.path(),
        session,
        &occurrence,
        "ApplyPatch",
    );
    let jobs =
        deepseek_code_core::services::background_sessions::list(&data(root.path()), None).unwrap();
    assert!(
        jobs.iter()
            .all(|job| !matches!(job.status.as_str(), "starting" | "running" | "unresponsive")),
        "no owned process may survive: {jobs:?}"
    );
}

#[tokio::test]
async fn sdk_asks_for_the_hook_rewritten_patch_and_a_denial_changes_nothing() {
    let server = Server::start(false, Mode::ApplyPatchAllowed).await;
    let root = tempfile::tempdir().unwrap();
    prepare(root.path());
    let response = json!({"hookSpecificOutput":{
        "hookEventName":"PreToolUse",
        "permissionDecision":"allow",
        "updatedInput":{"patch":APPLY_PATCH_REWRITTEN}
    }})
    .to_string();
    let command = if cfg!(windows) {
        format!("[Console]::Out.Write('{}')", response.replace('\'', "''"))
    } else {
        format!("printf '%s' '{}'", response.replace('\'', "'\\''"))
    };
    let mut value = json!({});
    value["hooks"] = json!({"PreToolUse":[{"matcher":"ApplyPatch","hooks":[{"type":"command","shell":if cfg!(windows) {"powershell"} else {"sh"},"command":command}]}]});
    settings(root.path(), value);

    let session = asked_session(root.path(), &["secret.txt"]);
    let mut client = Client::start(root.path(), &server, &["--resume", session.as_str()]);
    client.send(user("apply the patch")).await;
    let request = permission_request(&mut client).await;
    let occurrence = one_permission_request(&request);
    // The prompt describes the effective input, not the patch the model sent.
    assert_eq!(
        request["request"]["input"]["patch"], APPLY_PATCH_REWRITTEN,
        "{request}"
    );
    unchanged(root.path());

    client.send(answer(&request, "deny")).await;
    let frames = frames_until_result(&mut client).await;
    let result = frames.last().unwrap();
    assert_eq!(result["is_error"], false, "{result}");
    let denials = result["permission_denials"].as_array().unwrap();
    assert_eq!(denials.len(), 1, "{result}");
    assert_eq!(denials[0]["tool_name"], "ApplyPatch");
    assert_eq!(denials[0]["tool_input"]["patch"], APPLY_PATCH_REWRITTEN);
    assert_eq!(denials[0]["tool_use_id"], occurrence);
    unchanged(root.path());
    assert_eq!(tool_results(&frames, &occurrence).len(), 1);
    assert!(client.finish().await.0.success());
}

/// A provider that reuses one raw call id must still produce two independent
/// occurrences: separate permission requests, separate results and separate
/// persisted records. Approving the first must not authorize the second.
#[tokio::test]
async fn sdk_reused_provider_id_keeps_occurrences_prompts_and_records_independent() {
    let server = Server::start(false, Mode::ApplyPatchReusedId).await;
    let root = tempfile::tempdir().unwrap();
    prepare(root.path());
    // Both targets are under an explicit ask, so neither call can auto-approve.
    let session = asked_session(root.path(), &["a.txt", "secret.txt"]);
    let mut client = Client::start(root.path(), &server, &["--resume", session.as_str()]);
    client.send(user("apply both patches")).await;
    let mut frames = Vec::new();

    let first = permission_request_into(&mut client, &mut frames).await;
    let first_occurrence = one_permission_request(&first);
    assert_eq!(first["request"]["input"]["patch"], APPLY_PATCH_REUSED_FIRST);
    unchanged(root.path());
    client.send(answer(&first, "allow")).await;

    // The second call reuses the provider's raw id and must ask on its own.
    let second = permission_request_into(&mut client, &mut frames).await;
    let second_occurrence = one_permission_request(&second);
    assert_ne!(
        second_occurrence, first_occurrence,
        "each execution needs its own occurrence id"
    );
    assert_ne!(
        second["request_id"], first["request_id"],
        "each execution needs its own permission request"
    );
    assert_eq!(
        second["request"]["input"]["patch"],
        APPLY_PATCH_REUSED_SECOND
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
        "ONE\n",
        "only the approved first call may have run"
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("secret.txt")).unwrap(),
        "secret\n",
        "the second call waits for its own answer"
    );

    client.send(answer(&second, "deny")).await;
    frames_until_result_into(&mut client, &mut frames).await;
    let result = frames.last().unwrap().clone();
    assert_eq!(result["is_error"], false, "{result}");
    assert_eq!(
        std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
        "ONE\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("secret.txt")).unwrap(),
        "secret\n",
        "denying the second occurrence must leave its file unchanged"
    );
    let denials = result["permission_denials"].as_array().unwrap();
    assert_eq!(denials.len(), 1, "{result}");
    assert_eq!(denials[0]["tool_use_id"], second_occurrence, "{result}");
    assert_eq!(denials[0]["tool_input"]["patch"], APPLY_PATCH_REUSED_SECOND);
    assert_eq!(tool_results(&frames, &first_occurrence).len(), 1);
    assert_eq!(tool_results(&frames, &second_occurrence).len(), 1);

    let session = result["session_id"].as_str().unwrap();
    let calls = persisted_tool_calls(root.path(), session);
    let applies: Vec<_> = calls
        .iter()
        .filter(|call| call.name == "ApplyPatch")
        .collect();
    assert_eq!(
        applies.len(),
        2,
        "both executions are recorded: {applies:?}"
    );
    let row = |occurrence: &str| {
        applies
            .iter()
            .find(|call| call.id.ends_with(&format!(":{occurrence}")))
            .unwrap_or_else(|| panic!("no persisted row for {occurrence}: {applies:?}"))
    };
    assert!(!row(&first_occurrence).is_error);
    assert!(
        row(&second_occurrence).is_error,
        "the denied second call must not persist as success: {:?}",
        row(&second_occurrence).output
    );

    // Provider-facing pairing still uses the one reused raw id for both calls.
    {
        let requests = server.fixture.requests.lock().unwrap();
        let body = requests.last().expect("the final provider request");
        let ids: Vec<&str> = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "tool")
            .filter_map(|message| message["tool_call_id"].as_str())
            .collect();
        assert_eq!(ids, vec![APPLY_PATCH_PROVIDER_ID, APPLY_PATCH_PROVIDER_ID]);
    }
    assert!(client.finish().await.0.success());
}
