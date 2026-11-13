//! Real TaskGet observations of the two backends that round 01 only covered
//! through shared fixtures: a script Workflow started by the real Workflow
//! tool, and a background Agent spawned by the real delegation path.
//!
//! One local mock provider serves the parent turn. The background Agent's
//! child is pinned to a second model id on the same mock so its request can be
//! answered separately; no external provider or credential is used.
use axum::{extract::State, response::IntoResponse, routing::post, Json, Router};
use serde_json::{json, Value};
use std::{
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::process::Command;

fn tool_names(body: &Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tool| {
            tool["function"]["name"]
                .as_str()
                .or_else(|| tool["name"].as_str())
                .map(str::to_owned)
        })
        .collect()
}

fn tool_results(body: &Value) -> Vec<Value> {
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|message| message["role"] == "tool")
        .filter_map(|message| message["content"].as_str())
        .filter_map(|content| serde_json::from_str(content).ok())
        .collect()
}

/// The actual request must carry exactly one of each task built-in and no
/// retired TaskOutput, while the two backends under test are enabled.
fn assert_task_tools(body: &Value) {
    let tools = tool_names(body);
    for name in ["TaskCreate", "TaskGet", "TaskUpdate", "TaskList"] {
        assert_eq!(
            tools.iter().filter(|tool| tool.as_str() == name).count(),
            1,
            "{name} must appear exactly once: {tools:?}"
        );
    }
    assert!(
        !tools.iter().any(|tool| tool == "TaskOutput"),
        "TaskOutput must not be advertised: {tools:?}"
    );
    // The actual request schema must expose dependency replacement.
    let update = body["tools"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|tool| tool["function"]["name"] == "TaskUpdate" || tool["name"] == "TaskUpdate")
        .expect("the request did not carry TaskUpdate");
    let parameters = if update["function"]["parameters"].is_object() {
        &update["function"]["parameters"]
    } else {
        &update["parameters"]
    };
    assert_eq!(
        parameters["properties"]["setBlockedBy"]["type"], "array",
        "setBlockedBy missing from the outgoing TaskUpdate schema: {update}"
    );
    assert_eq!(
        parameters["properties"]["setBlockedBy"]["items"]["type"],
        "string"
    );
    for name in ["Workflow", "Agent"] {
        assert_eq!(
            tools.iter().filter(|tool| tool.as_str() == name).count(),
            1,
            "{name} must be enabled for this fixture: {tools:?}"
        );
    }
}

fn tool_call_frame(id: &str, name: &str, arguments: &Value) -> Vec<Value> {
    vec![
        json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":name,"arguments":arguments.to_string()}}]},"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
    ]
}

fn text_frames(text: &str) -> Vec<Value> {
    vec![
        json!({"choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
    ]
}

fn stream(frames: Vec<Value>) -> axum::response::Response {
    let mut stream: String = frames
        .into_iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect();
    stream.push_str("data: [DONE]\n\n");
    ([("content-type", "text/event-stream")], stream).into_response()
}

fn provider_config(base: &str) -> Value {
    let model = |id: &str| {
        json!({"id":id,"name":id,"enabled":true,"context_window":128000,"max_tokens":4096,
            "supports_web_search":false,"supports_vision":false})
    };
    json!([{
        "id":"fixture","name":"Fixture","base_url":base,"api_key":"fixture-only",
        "api_format":"openai","enabled":true,
        "models":[model("fixture-model"), model("fixture-child")]
    }])
}

/// The four task tools and both backends are enabled; the default request
/// would carry only TaskGet/TaskStop.
fn write_customization(data: &std::path::Path) {
    let switch = || json!({"enabled":true,"on_demand":true});
    let customization = json!({"tool_loading":{
        "TaskCreate":switch(),"TaskGet":switch(),"TaskUpdate":switch(),"TaskList":switch(),
        "Agent":switch(),"Workflow":switch()
    }});
    std::fs::write(data.join("customization.json"), customization.to_string()).unwrap();
}

struct CliFixture {
    root: tempfile::TempDir,
    data: std::path::PathBuf,
    server: tokio::task::JoinHandle<()>,
}

async fn start_cli_fixture(
    responder: axum::routing::MethodRouter<FixtureState>,
) -> (CliFixture, Fixture) {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let fixture = Fixture::new(data.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::fs::write(
        data.join("providers.json"),
        provider_config(&base).to_string(),
    )
    .unwrap();
    write_customization(&data);
    let app = Router::new()
        .route("/v1/chat/completions", post(responder))
        .with_state(fixture.state.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (CliFixture { root, data, server }, fixture)
}

/// Shared handles for both fixtures.
#[derive(Clone)]
struct FixtureState {
    /// The isolated data directory, so a responder can observe real state.
    data: std::path::PathBuf,
    requests: Arc<Mutex<Vec<Value>>>,
    slots: Arc<Mutex<std::collections::BTreeMap<String, Value>>>,
}

#[derive(Clone)]
struct Fixture {
    state: FixtureState,
}

impl Fixture {
    fn new(data: std::path::PathBuf) -> Self {
        Self {
            state: FixtureState {
                data,
                requests: Arc::new(Mutex::new(vec![])),
                slots: Arc::new(Mutex::new(Default::default())),
            },
        }
    }

    fn record_request(&self, body: &Value) {
        self.state.requests.lock().unwrap().push(body.clone());
    }

    fn store(&self, key: &str, value: Value) {
        self.state.slots.lock().unwrap().insert(key.into(), value);
    }

    fn slot(&self, key: &str) -> Value {
        self.state
            .slots
            .lock()
            .unwrap()
            .get(key)
            .cloned()
            .unwrap_or_else(|| panic!("missing fixture slot {key}"))
    }
}

async fn run_cli(fixture: &CliFixture, session: &str) -> std::process::Output {
    let mut cmd = Command::new(
        std::env::var_os("CLAUDE_SDK_TEST_EXE").unwrap_or_else(|| env!("CARGO_BIN_EXE_dcl").into()),
    );
    cmd.args(["--print", "--output-format", "stream-json", "--data-dir"])
        .arg(&fixture.data)
        .arg("--path")
        .arg(fixture.root.path())
        .args([
            "--provider",
            "fixture",
            "--model",
            "fixture-model",
            "--permission-mode",
            "bypassPermissions",
            "--session-id",
            session,
            "fixture task",
        ])
        .stdin(Stdio::null())
        .kill_on_drop(true);
    for (key, _) in std::env::vars() {
        if key.starts_with("DEEPSEEK_CODE") {
            cmd.env_remove(key);
        }
    }
    let output = tokio::time::timeout(Duration::from_secs(120), cmd.output())
        .await
        .expect("CLI fixture timed out")
        .unwrap();
    fixture.server.abort();
    output
}

fn assert_single_success(output: &std::process::Output) -> Vec<Value> {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events: Vec<Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.starts_with('{'))
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let results: Vec<_> = events
        .iter()
        .filter(|event| event["type"] == "result")
        .collect();
    assert_eq!(results.len(), 1, "{events:?}");
    assert_eq!(results[0]["is_error"], false, "{}", results[0]);
    events
}

const WORKFLOW_WAITING: &str = r#"export const meta = { name: 'fixture-flow', description: 'TaskGet workflow fixture' }
log('workflow-start')
const end = Date.now() + 2500
while (Date.now() < end) {}
return { ok: true, marker: 'workflow-finished' }
"#;

const WORKFLOW_LONG: &str = r#"export const meta = { name: 'fixture-long', description: 'TaskGet cancellation fixture' }
log('workflow-long-start')
const end = Date.now() + 60000
while (Date.now() < end) {}
return { ok: true }
"#;

async fn workflow_respond(
    State(fixture): State<FixtureState>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    let fixture = Fixture { state: fixture };
    fixture.record_request(&body);
    assert_eq!(body["stream"], true);
    let results = tool_results(&body);
    let step = results.len();
    let frames = match step {
        0 => {
            assert_task_tools(&body);
            tool_call_frame(
                "call-workflow-waiting",
                "Workflow",
                &json!({"script": WORKFLOW_WAITING}),
            )
        }
        1 => {
            assert_task_tools(&body);
            let started = results[0].clone();
            assert!(
                started["task_id"].as_str().is_some(),
                "Workflow start result: {started}"
            );
            fixture.store("workflow_waiting", started.clone());
            tool_call_frame(
                "call-get-timeout",
                "TaskGet",
                &json!({"task_id":started["task_id"],"block":true,"timeout":100}),
            )
        }
        2 => {
            assert_task_tools(&body);
            fixture.store("timeout_result", results[1].clone());
            let started = fixture.slot("workflow_waiting");
            tool_call_frame(
                "call-get-completed",
                "TaskGet",
                &json!({"task_id":started["task_id"],"block":true,"timeout":10000}),
            )
        }
        3 => {
            assert_task_tools(&body);
            fixture.store("completed_result", results[2].clone());
            tool_call_frame(
                "call-workflow-long",
                "Workflow",
                &json!({"script": WORKFLOW_LONG}),
            )
        }
        4 => {
            assert_task_tools(&body);
            let started = results[3].clone();
            assert!(
                started["task_id"].as_str().is_some(),
                "second Workflow start result: {started}"
            );
            fixture.store("workflow_long", started.clone());
            tool_call_frame(
                "call-stop",
                "TaskStop",
                &json!({"task_id":started["task_id"]}),
            )
        }
        5 => {
            assert_task_tools(&body);
            fixture.store("stop_result", results[4].clone());
            let started = fixture.slot("workflow_long");
            tool_call_frame(
                "call-get-cancelled",
                "TaskGet",
                &json!({"task_id":started["task_id"],"block":true,"timeout":5000}),
            )
        }
        _ => {
            if let Some(result) = results.get(5) {
                fixture.store("cancelled_result", result.clone());
            }
            text_frames("workflow fixtures observed")
        }
    };
    stream(frames)
}

#[tokio::test]
async fn workflow_task_get_returns_real_output_times_out_and_stops_without_cancelling_on_timeout() {
    let (fixture, state) = start_cli_fixture(post(workflow_respond)).await;
    let session = uuid::Uuid::new_v4().to_string();
    let output = run_cli(&fixture, &session).await;
    assert_single_success(&output);
    assert!(
        state.state.requests.lock().unwrap().len() >= 7,
        "the scripted parent turn did not issue every expected call"
    );

    let started = state.slot("workflow_waiting");
    let task_id = started["task_id"].as_str().expect("Workflow task ID");
    assert_eq!(started["status"], "running", "{started}");
    assert_eq!(started["run_id"].as_str().is_some(), true, "{started}");

    // A timed-out read reports the running workflow and leaves it alive.
    let timed_out = state.slot("timeout_result");
    assert_eq!(timed_out["task_id"], task_id, "{timed_out}");
    assert_eq!(timed_out["status"], "running", "{timed_out}");
    assert_eq!(timed_out["retrieval_status"], "timeout", "{timed_out}");
    assert!(
        timed_out["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("workflow-start"),
        "the timed-out read did not return output already written: {timed_out}"
    );

    // The blocking read observes the real child's completion and its output.
    let completed = state.slot("completed_result");
    assert_eq!(completed["task_id"], task_id, "{completed}");
    assert_eq!(completed["status"], "completed", "{completed}");
    assert_eq!(completed["retrieval_status"], "success", "{completed}");
    let stdout = completed["stdout"].as_str().unwrap_or_default();
    assert!(
        stdout.contains("workflow-start") && stdout.contains("workflow-finished"),
        "TaskGet did not return the real workflow output: {completed}"
    );
    let task_directory = fixture
        .data
        .join("shell-tasks")
        .join(&session)
        .join(task_id);
    assert!(
        !task_directory.join("cancel").exists(),
        "the TaskGet timeout cancelled the workflow"
    );
    // The output came from the real `dcl workflow-run` child's managed log.
    let managed_log = std::fs::read_to_string(task_directory.join("stdout.log")).unwrap();
    assert!(
        managed_log.contains("workflow-start") && managed_log.contains("workflow-finished"),
        "the managed workflow log was not the observed output: {managed_log}"
    );

    // A second workflow is explicitly stopped.
    let long = state.slot("workflow_long");
    let long_id = long["task_id"].as_str().expect("second task ID");
    let stopped = state.slot("stop_result");
    assert_eq!(stopped["task_id"], long_id, "{stopped}");
    assert_eq!(stopped["cancellation_requested"], true, "{stopped}");
    let cancelled = state.slot("cancelled_result");
    assert_eq!(cancelled["task_id"], long_id, "{cancelled}");
    assert_eq!(cancelled["status"], "cancelled", "{cancelled}");
    assert_eq!(cancelled["retrieval_status"], "success", "{cancelled}");
    let long_directory = fixture
        .data
        .join("shell-tasks")
        .join(&session)
        .join(long_id);
    assert!(
        long_directory.join("cancel").exists(),
        "TaskStop did not request cancellation"
    );

    // The workflow records stay session-scoped through the ordinary task
    // storage path.
    let foreign = uuid::Uuid::new_v4().to_string();
    let error = deepseek_code_core::tools::tasks::execute_for_session_async(
        "TaskGet",
        json!({"task_id":task_id}),
        &fixture.data,
        &foreign,
    )
    .await
    .unwrap_err();
    assert!(
        error.to_string().contains("Task not found"),
        "a foreign session read the workflow task: {error}"
    );

    // The real Workflow runner persisted the run it announced.
    let run_id = started["run_id"].as_str().unwrap();
    let run_dir = fixture.data.join("workflows").join(&session).join(run_id);
    let status: Value =
        serde_json::from_slice(&std::fs::read(run_dir.join("status.json")).unwrap()).unwrap();
    assert_eq!(status["status"], "completed", "{status}");
    let script = std::fs::read_to_string(run_dir.join("script.js")).unwrap();
    assert!(script.contains("workflow-finished"), "{script}");
    let result: Value =
        serde_json::from_slice(&std::fs::read(run_dir.join("result.json")).unwrap()).unwrap();
    assert_eq!(result["marker"], "workflow-finished", "{result}");
}

async fn agent_respond(
    State(fixture): State<FixtureState>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    let fixture = Fixture { state: fixture };
    fixture.record_request(&body);
    assert_eq!(body["stream"], true);
    // The spawned child is pinned to the second model id by the Agent tool.
    if body["model"] == "fixture-child" {
        tokio::time::sleep(Duration::from_millis(1_500)).await;
        return stream(text_frames("child-report-marker"));
    }
    let results = tool_results(&body);
    let step = results.len();
    let frames = match step {
        0 => {
            assert_task_tools(&body);
            tool_call_frame(
                "call-agent",
                "Agent",
                &json!({
                    "description": "fixture child",
                    "prompt": "Reply with the exact marker",
                    "model": "fixture-child",
                    "run_in_background": true
                }),
            )
        }
        1 => {
            assert_task_tools(&body);
            let started = results[0].clone();
            assert_eq!(started["backgrounded"], true, "{started}");
            assert!(
                started["agent_id"].as_str().is_some(),
                "Agent start result: {started}"
            );
            fixture.store("agent_started", started.clone());
            tool_call_frame(
                "call-get-running",
                "TaskGet",
                &json!({"task_id":started["agent_id"],"block":false}),
            )
        }
        2 => {
            assert_task_tools(&body);
            fixture.store("running_result", results[1].clone());
            let started = fixture.slot("agent_started");
            tool_call_frame(
                "call-get-timeout",
                "TaskGet",
                &json!({"task_id":started["agent_id"],"block":true,"timeout":100}),
            )
        }
        3 => {
            assert_task_tools(&body);
            fixture.store("timeout_result", results[2].clone());
            let started = fixture.slot("agent_started");
            tool_call_frame(
                "call-get-completed",
                "TaskGet",
                &json!({"task_id":started["agent_id"],"block":true,"timeout":15000}),
            )
        }
        _ => {
            if let Some(result) = results.get(3) {
                fixture.store("completed_result", result.clone());
            }
            text_frames("agent fixture observed")
        }
    };
    stream(frames)
}

#[tokio::test]
async fn background_agent_task_get_reports_real_child_progress_report_and_completion() {
    let (fixture, state) = start_cli_fixture(post(agent_respond)).await;
    let session = uuid::Uuid::new_v4().to_string();
    let output = run_cli(&fixture, &session).await;
    assert_single_success(&output);

    let started = state.slot("agent_started");
    let job_id = started["agent_id"].as_str().expect("agent job ID");
    let child_session = started["session_id"].as_str().expect("child session ID");
    assert_ne!(child_session, session, "{started}");

    // The immediate read sees the real job while it is still running.
    let running = state.slot("running_result");
    assert_eq!(running["task_id"], job_id, "{running}");
    assert!(
        matches!(running["status"].as_str(), Some("starting" | "running")),
        "{running}"
    );
    assert_eq!(running["retrieval_status"], "not_ready", "{running}");

    // A wait deadline reports the timeout without stopping the child.
    let timed_out = state.slot("timeout_result");
    assert_eq!(timed_out["task_id"], job_id, "{timed_out}");
    assert_eq!(timed_out["retrieval_status"], "timeout", "{timed_out}");
    assert!(
        matches!(timed_out["status"].as_str(), Some("starting" | "running")),
        "{timed_out}"
    );

    // The blocking read returns the child's real report and exit status.
    let completed = state.slot("completed_result");
    assert_eq!(completed["task_id"], job_id, "{completed}");
    assert_eq!(completed["status"], "completed", "{completed}");
    assert_eq!(completed["exit_code"], 0, "{completed}");
    assert!(completed["error"].is_null(), "{completed}");
    assert_eq!(completed["retrieval_status"], "success", "{completed}");
    assert!(
        completed["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("child-report-marker"),
        "TaskGet did not return the child's report: {completed}"
    );

    let job_directory = fixture.data.join("background-tasks").join(job_id);
    assert!(
        !job_directory.join("cancel").exists(),
        "the TaskGet timeout cancelled the background agent"
    );
    let job_state: Value =
        serde_json::from_slice(&std::fs::read(job_directory.join("state.json")).unwrap()).unwrap();
    assert_eq!(job_state["parent_session_id"], session, "{job_state}");
    assert_eq!(job_state["session_id"], child_session, "{job_state}");
    assert_eq!(job_state["status"], "completed", "{job_state}");
    assert_eq!(job_state["exit_code"], 0, "{job_state}");
    assert!(job_state["finished_ms"].as_u64().is_some(), "{job_state}");
    // The job was run by a real supervisor and child process.
    assert!(
        job_state["supervisor_pid"].as_u64().is_some(),
        "{job_state}"
    );
    assert!(job_state["child_pid"].as_u64().is_some(), "{job_state}");
    let launch: Value =
        serde_json::from_slice(&std::fs::read(job_directory.join("launch.json")).unwrap()).unwrap();
    let arguments = launch["arguments"].to_string();
    assert!(
        arguments.contains(child_session) && arguments.contains("fixture-child"),
        "the launched child was not the resumed fixture session: {launch}"
    );
    assert!(
        std::fs::metadata(job_directory.join("output.jsonl"))
            .unwrap()
            .len()
            > 0,
        "the real child produced no stream output"
    );

    // The real job belongs to the owning session only.
    let foreign = uuid::Uuid::new_v4().to_string();
    let error = deepseek_code_core::tools::tasks::execute_for_session_async(
        "TaskGet",
        json!({"task_id":job_id}),
        &fixture.data,
        &foreign,
    )
    .await
    .unwrap_err();
    assert!(
        error.to_string().contains("another session"),
        "a foreign session read the background agent: {error}"
    );
}

/// Insert a genuine prior assistant turn ahead of the child session's current
/// turn.
///
/// Message history is ordered by `sort_order`; `created_at` is descriptive only
/// (`job_report` in core selects `ORDER BY sort_order DESC`). "Prior" therefore
/// means a lower `sort_order`, not an older timestamp. Appending at
/// `MAX(sort_order)+1` after the child has written its current turn would place
/// the seed *after* that turn and make it the reported final message.
///
/// The row is written at the front in one transaction that shifts the existing
/// rows up, so the result does not depend on when the seed physically runs
/// relative to the child's own writes. A child insert after the commit takes
/// `MAX(sort_order)+1`, which is still later than this row.
fn seed_prior_assistant_turn(data: &std::path::Path, session: &str, text: &str) {
    let mut conn =
        rusqlite::Connection::open(data.join(deepseek_code_core::runtime::DATABASE_FILE)).unwrap();
    conn.busy_timeout(Duration::from_secs(10)).unwrap();
    let mut attempts = 0;
    loop {
        let result = (|| -> rusqlite::Result<()> {
            let tx = conn.transaction()?;
            tx.execute(
                "UPDATE messages SET sort_order = sort_order + 1 WHERE conversation_id=?1",
                [session],
            )?;
            tx.execute(
                "INSERT INTO messages (id, conversation_id, role, content, thinking, created_at, is_compact_boundary, sort_order) VALUES (?1,?2,'assistant',?3,NULL,?4,0,0)",
                rusqlite::params![
                    uuid::Uuid::new_v4().to_string(),
                    session,
                    text,
                    "2026-01-01T00:00:00Z"
                ],
            )?;
            tx.commit()
        })();
        match result {
            Ok(()) => return,
            Err(error)
                if error.sqlite_error_code() == Some(rusqlite::ErrorCode::DatabaseBusy)
                    && attempts < 40 =>
            {
                attempts += 1;
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => panic!("could not seed a prior assistant turn: {error}"),
        }
    }
}

/// Release file that lets the paged child's Bash call finish. Dropping it on an
/// assertion failure also cancels every job in this isolated data directory, so
/// a failed assertion cannot leave an owned child looping.
struct GateRelease(std::path::PathBuf);

impl Drop for GateRelease {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.0, b"release");
        let data = self
            .0
            .parent()
            .expect("the gate lives directly in the data directory");
        if let Ok(entries) = std::fs::read_dir(data.join("background-tasks")) {
            for entry in entries.flatten() {
                let _ = std::fs::write(entry.path().join("cancel"), b"1");
            }
        }
    }
}

fn gate_path(data: &std::path::Path) -> std::path::PathBuf {
    data.join("paged-agent-gate")
}

/// The content of the tool message that answers the child's own Bash call.
///
/// A real Bash result is a plain-text summary, not JSON, so the parent's
/// structured `tool_results` helper cannot be used to sequence the child.
fn child_bash_result(body: &Value) -> Option<String> {
    let messages = body["messages"].as_array()?;
    let id = messages.iter().find_map(|message| {
        message["tool_calls"].as_array()?.iter().find_map(|call| {
            if call["function"]["name"] == "Bash" {
                call["id"].as_str().map(str::to_owned)
            } else {
                None
            }
        })
    })?;
    messages.iter().rev().find_map(|message| {
        if message["role"] == "tool" && message["tool_call_id"] == id.as_str() {
            message["content"].as_str().map(str::to_owned)
        } else {
            None
        }
    })
}

/// Compact request trace for a failing run: roles, tool call ids/names and the
/// Bash result text, without dumping the whole system prompt.
fn child_request_summary(requests: &[Value]) -> String {
    let child: Vec<&Value> = requests
        .iter()
        .filter(|request| request["model"] == "fixture-child")
        .collect();
    let mut summary = String::new();
    for (index, request) in child.iter().enumerate() {
        if index >= 4 && index + 4 < child.len() {
            continue;
        }
        let messages = request["messages"].as_array().cloned().unwrap_or_default();
        let roles = messages
            .iter()
            .map(|message| message["role"].as_str().unwrap_or("?"))
            .collect::<Vec<_>>();
        let mut calls = Vec::new();
        let mut results = Vec::new();
        for message in &messages {
            for call in message["tool_calls"].as_array().into_iter().flatten() {
                calls.push(format!(
                    "{}:{}",
                    call["id"].as_str().unwrap_or("?"),
                    call["function"]["name"].as_str().unwrap_or("?")
                ));
            }
            if message["role"] == "tool" {
                results.push(format!(
                    "{} => {}",
                    message["tool_call_id"].as_str().unwrap_or("?"),
                    message["content"]
                        .as_str()
                        .unwrap_or_default()
                        .chars()
                        .take(160)
                        .collect::<String>()
                ));
            }
        }
        summary.push_str(&format!(
            "#{index} roles={roles:?} calls={calls:?}\n  results={results:?}\n"
        ));
    }
    summary
}

/// Bounded readiness check: the real job is `running` and has already flushed
/// the text the pages are expected to observe. This observes actual state; it
/// is not an elapsed-time guess that can flake under load.
async fn wait_for_running_progress(data: &std::path::Path, job_id: &str) -> Result<(), String> {
    let directory = data.join("background-tasks").join(job_id);
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let status = std::fs::read_to_string(directory.join("state.json"))
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|value| value["status"].as_str().map(str::to_owned));
        let progress = std::fs::read_to_string(directory.join("output.jsonl"))
            .map(|text| text.contains("child-progress"))
            .unwrap_or(false);
        if status.as_deref() == Some("running") && progress {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "owned child did not reach running progress: status={status:?} progress={progress}"
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A real background Agent whose child session already has an assistant turn.
/// The child emits text and then blocks on a fixture-owned gate, so the job
/// stays `running` until the parent has captured both pages.
async fn agent_paging_respond(
    State(fixture): State<FixtureState>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    let fixture = Fixture { state: fixture };
    fixture.record_request(&body);
    assert_eq!(body["stream"], true);
    if body["model"] == "fixture-child" {
        if let Some(result) = child_bash_result(&body) {
            assert!(
                result.contains("gate-released"),
                "the held Bash call did not observe the gate release: {result}"
            );
            return stream(text_frames("child-final-marker"));
        }
        let gate = gate_path(&fixture.state.data);
        let command = format!(
            "i=0; while [ ! -e '{}' ] && [ $i -lt 1200 ]; do i=$((i+1)); sleep 0.05; done; echo gate-released",
            gate.display()
        );
        let frames = vec![
            json!({"choices":[{"index":0,"delta":{"content":"child-progress"},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"child-hold","type":"function","function":{"name":"Bash","arguments":json!({"command":command,"description":"Wait for the paging fixture gate"}).to_string()}}]},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
        ];
        return stream(frames);
    }
    let results = tool_results(&body);
    let step = results.len();
    let frames = match step {
        0 => {
            assert_task_tools(&body);
            tool_call_frame(
                "call-agent",
                "Agent",
                &json!({
                    "description": "paged fixture child",
                    "prompt": "Reply with the exact marker",
                    "model": "fixture-child",
                    "run_in_background": true
                }),
            )
        }
        1 => {
            let started = results[0].clone();
            let child = started["session_id"]
                .as_str()
                .expect("child session ID")
                .to_owned();
            fixture.store("paged_agent", started.clone());
            let job_id = started["agent_id"]
                .as_str()
                .expect("agent job ID")
                .to_owned();
            // Wait until the child's current turn really exists, then seed the
            // prior turn. Doing it in this order exercises the case that the old
            // `MAX(sort_order)+1` seed got wrong: a prior turn written after the
            // current one must still sort before it.
            if let Err(error) = wait_for_running_progress(&fixture.state.data, &job_id).await {
                panic!("{error}");
            }
            let data = fixture.state.data.clone();
            tokio::task::spawn_blocking(move || {
                seed_prior_assistant_turn(&data, &child, "PRIOR-ASSISTANT-HISTORY");
            })
            .await
            .unwrap();
            tool_call_frame(
                "call-page-1",
                "TaskGet",
                &json!({"task_id":job_id,"max_output_bytes":200}),
            )
        }
        2 => {
            let page = results[1].clone();
            fixture.store("paged_first", page.clone());
            tool_call_frame(
                "call-page-2",
                "TaskGet",
                &json!({"task_id":page["task_id"],"cursor":page["next_cursor"],"max_output_bytes":200}),
            )
        }
        3 => {
            let page = results[2].clone();
            fixture.store("paged_second", page.clone());
            // Both pages are captured: release the child and wait for it.
            std::fs::write(gate_path(&fixture.state.data), b"release").unwrap();
            tool_call_frame(
                "call-complete",
                "TaskGet",
                &json!({"task_id":page["task_id"],"block":true,"timeout":15000}),
            )
        }
        4 => {
            fixture.store("paged_completed", results[3].clone());
            text_frames("paged agent observed")
        }
        _ => text_frames("paged agent observed"),
    };
    stream(frames)
}

#[tokio::test]
async fn background_agent_task_get_pages_while_a_prior_assistant_turn_exists() {
    let (fixture, state) = start_cli_fixture(post(agent_paging_respond)).await;
    // Released on every exit path, so an assertion cannot leave the child held.
    let _gate = GateRelease(gate_path(&fixture.data));
    let session = uuid::Uuid::new_v4().to_string();
    let output = run_cli(&fixture, &session).await;
    assert_single_success(&output);

    let started = state.slot("paged_agent");
    let job_id = started["agent_id"].as_str().expect("agent job ID");
    let child_session = started["session_id"].as_str().expect("child session ID");
    assert_ne!(child_session, session, "{started}");

    // The running job page is the current event stream, not the stored prior
    // assistant turn, and it carries a valid cursor.
    let first = state.slot("paged_first");
    assert_eq!(first["output_mode"], "page", "{first}");
    assert_eq!(first["status"], "running", "{first}");
    assert_eq!(first["retrieval_status"], "not_ready", "{first}");
    assert!(
        first["next_cursor"].as_str().is_some(),
        "a running job must return a cursor: {first}"
    );
    assert!(
        !first["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("PRIOR-ASSISTANT-HISTORY"),
        "the prior assistant turn leaked into the running page: {first}"
    );
    let second = state.slot("paged_second");
    assert_eq!(second["output_mode"], "page", "{second}");
    assert_eq!(second["status"], "running", "{second}");
    assert_eq!(second["retrieval_status"], "not_ready", "{second}");
    let mut paged = first["stdout"].as_str().unwrap_or_default().to_owned();
    paged.push_str(second["stdout"].as_str().unwrap_or_default());
    assert!(
        !paged.contains("PRIOR-ASSISTANT-HISTORY"),
        "the prior assistant turn leaked into the pages: {paged:?}"
    );
    assert!(
        paged.contains("child-progress"),
        "the pages did not see the running child's current output: first={first} second={second}"
    );

    // Completion still returns the child's final report.
    let completed = state.slot("paged_completed");
    if completed["status"] != "completed" {
        let directory = fixture.data.join("background-tasks").join(job_id);
        let job_state = std::fs::read_to_string(directory.join("state.json")).unwrap_or_default();
        let stderr = std::fs::read_to_string(directory.join("stderr.log")).unwrap_or_default();
        let stream = std::fs::read_to_string(directory.join("output.jsonl")).unwrap_or_default();
        let requests = state.state.requests.lock().unwrap().clone();
        let trace = child_request_summary(&requests);
        let gate_visible = gate_path(&fixture.data).exists();
        panic!(
            "the owned child did not complete: {completed}\nstate={job_state}\nstderr={stderr}\noutput={stream}\ngate_visible={gate_visible}\nchild_requests:\n{trace}"
        );
    }
    assert_eq!(completed["status"], "completed", "{completed}");
    assert_eq!(completed["retrieval_status"], "success", "{completed}");
    assert!(
        completed["stdout"]
            .as_str()
            .unwrap_or_default()
            .contains("child-final-marker"),
        "the completed read lost the child report: {completed}"
    );

    // The seeded turn really was in the child session while the job ran, and it
    // really sorts before the current turn: history order is `sort_order`, not
    // `created_at`.
    let conn = rusqlite::Connection::open(
        fixture
            .data
            .join(deepseek_code_core::runtime::DATABASE_FILE),
    )
    .unwrap();
    let seeded: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM messages WHERE conversation_id=?1 AND content='PRIOR-ASSISTANT-HISTORY'",
            [child_session],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(seeded, 1);
    let seeded_order: i64 = conn
        .query_row(
            "SELECT sort_order FROM messages WHERE conversation_id=?1 AND content='PRIOR-ASSISTANT-HISTORY'",
            [child_session],
            |row| row.get(0),
        )
        .unwrap();
    let current_order: i64 = conn
        .query_row(
            "SELECT sort_order FROM messages WHERE conversation_id=?1 AND role='assistant' AND content LIKE 'child-progress%'",
            [child_session],
            |row| row.get(0),
        )
        .unwrap();
    let (newest_order, newest_content): (i64, String) = conn
        .query_row(
            "SELECT sort_order, content FROM messages WHERE conversation_id=?1 AND role='assistant' AND content<>'' ORDER BY sort_order DESC LIMIT 1",
            [child_session],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    eprintln!("ordering: seeded={seeded_order} current={current_order} newest={newest_order}");
    assert!(
        seeded_order < current_order,
        "the seeded prior turn does not sort before the current turn: seeded={seeded_order} current={current_order}"
    );
    assert_eq!(
        newest_order, current_order,
        "the newest assistant row is not the child's current turn: newest={newest_order} current={current_order}"
    );
    assert!(
        newest_content.contains("child-final-marker"),
        "the newest assistant row is not the child's final report: {newest_content:?}"
    );

    // The paged prefix is a prefix of the real append-only event text.
    let raw = std::fs::read_to_string(
        fixture
            .data
            .join("background-tasks")
            .join(job_id)
            .join("output.jsonl"),
    )
    .unwrap();
    let mut snapshot = String::new();
    let mut first_text = true;
    for line in raw.lines() {
        let Ok(event) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if let Some(text) = event["result"].as_str().or_else(|| event["delta"].as_str()) {
            if !first_text {
                snapshot.push('\n');
            }
            snapshot.push_str(text);
            first_text = false;
        }
    }
    if !paged.is_empty() {
        assert!(
            snapshot.starts_with(&paged),
            "the pages are not a prefix of the current output: {paged:?} vs {snapshot:?}"
        );
    }
}
