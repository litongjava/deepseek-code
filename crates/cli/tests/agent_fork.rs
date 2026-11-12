//! A real CLI session forks itself through the Agent tool. The local model
//! fixture records every request so the child request can be told apart from
//! the parent's by the directive line, and the copied history and the child
//! session row are both verified.
use axum::{extract::State, response::IntoResponse, routing::post, Json, Router};
use serde_json::{json, Value};
use std::{
    path::Path,
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::process::Command;

const FORK_DIRECTIVE: &str = "You are a fork of this session: you have its full history above, and this message is your directive. Work on it alone and finish with a short report; the parent session will receive it.";

#[derive(Clone, Default)]
struct Fixture {
    requests: Arc<Mutex<Vec<Value>>>,
    fork_requests: Arc<Mutex<Vec<Value>>>,
    parent_count: Arc<Mutex<usize>>,
    fork_count: Arc<Mutex<usize>>,
    agent_result: Arc<Mutex<Option<Value>>>,
    nested_result: Arc<Mutex<Option<String>>>,
    /// Answer the fork's first request with another fork call.
    nested_fork: bool,
}

fn next(counter: &Arc<Mutex<usize>>) -> usize {
    let mut count = counter.lock().unwrap();
    let index = *count;
    *count += 1;
    index
}

fn last_tool_result(body: &Value) -> Option<Value> {
    body["messages"]
        .as_array()?
        .iter()
        .rev()
        .find(|message| message["role"] == "tool")
        .and_then(|message| message["content"].as_str())
        .and_then(|content| serde_json::from_str(content).ok())
}

/// The raw text of the last tool result; errors are plain text, not JSON.
fn last_tool_content(body: &Value) -> Option<String> {
    body["messages"]
        .as_array()?
        .iter()
        .rev()
        .find(|message| message["role"] == "tool")
        .and_then(|message| message["content"].as_str())
        .map(str::to_owned)
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

async fn respond(
    State(fixture): State<Fixture>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    assert_eq!(body["stream"], true);
    let serialized = serde_json::to_string(&body).unwrap();
    let is_fork_request = serialized.contains(FORK_DIRECTIVE);
    {
        fixture.requests.lock().unwrap().push(body.clone());
        if is_fork_request {
            fixture.fork_requests.lock().unwrap().push(body.clone());
        }
    }
    let frames = if is_fork_request {
        match next(&fixture.fork_count) {
            0 if fixture.nested_fork => tool_call_frame(
                "call-nested-fork",
                "Agent",
                &json!({"description":"nested fork check","prompt":"nested directive","subagent_type":"fork"}),
            ),
            0 => text_frames("child answer"),
            _ => {
                if let Some(result) = last_tool_content(&body) {
                    *fixture.nested_result.lock().unwrap() = Some(result);
                }
                text_frames("child done")
            }
        }
    } else {
        match next(&fixture.parent_count) {
            0 => text_frames("noted"),
            1 => tool_call_frame(
                "call-search",
                "ToolSearch",
                &json!({"query":"select:Agent"}),
            ),
            2 => tool_call_frame(
                "call-fork",
                "Agent",
                &json!({"description":"fork check","prompt":"say the marker","subagent_type":"fork"}),
            ),
            _ => {
                if let Some(result) = last_tool_result(&body) {
                    *fixture.agent_result.lock().unwrap() = Some(result);
                }
                text_frames("parent done")
            }
        }
    };
    let mut stream: String = frames
        .into_iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect();
    stream.push_str("data: [DONE]\n\n");
    ([("content-type", "text/event-stream")], stream).into_response()
}

fn parent_command(
    data: &Path,
    workspace: &Path,
    session: &str,
    resume: bool,
    prompt: &str,
) -> Command {
    let mut cmd = Command::new(
        std::env::var_os("CLAUDE_SDK_TEST_EXE").unwrap_or_else(|| env!("CARGO_BIN_EXE_dcl").into()),
    );
    cmd.args(["--print", "--output-format", "stream-json", "--data-dir"])
        .arg(data)
        .arg("--path")
        .arg(workspace)
        .args([
            "--provider",
            "fixture",
            "--model",
            "fixture-model",
            "--permission-mode",
            "bypassPermissions",
        ]);
    if resume {
        cmd.arg("--resume").arg(session);
    } else {
        cmd.arg("--session-id").arg(session);
    }
    cmd.arg(prompt).stdin(Stdio::null()).kill_on_drop(true);
    for (key, _) in std::env::vars() {
        if key.starts_with("DEEPSEEK_CODE") {
            cmd.env_remove(key);
        }
    }
    cmd
}

async fn run(mut cmd: Command) -> std::process::Output {
    tokio::time::timeout(Duration::from_secs(60), cmd.output())
        .await
        .expect("dcl turn timed out")
        .expect("dcl turn failed")
}

async fn wait_until(condition: impl Fn() -> bool, what: &str) {
    for _ in 0..600 {
        if condition() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timed out waiting for {what}");
}

async fn fixture_server(fixture: Fixture) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/chat/completions", post(respond))
        .with_state(fixture);
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, server)
}

fn write_providers(data: &Path, base: &str) {
    std::fs::write(
        data.join("providers.json"),
        json!([{
            "id":"fixture","name":"Fixture","base_url":base,"api_key":"fixture-only",
            "api_format":"openai","enabled":true,
            "models":[{"id":"fixture-model","name":"Fixture","enabled":true,
                "context_window":128000,"max_tokens":4096,
                "supports_web_search":false,"supports_vision":false}]
        }])
        .to_string(),
    )
    .unwrap();
}

/// Three-parent-request setup: turn one answers text, turn two calls Agent
/// with `subagent_type: "fork"`, and the follow-up records the tool result.
async fn run_parent_turns(fixture: &Fixture, data: &Path, workspace: &Path, session: &str) {
    let first = run(parent_command(
        data,
        workspace,
        session,
        false,
        "remember PINEAPPLE-7",
    ))
    .await;
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let second = run(parent_command(
        data,
        workspace,
        session,
        true,
        "now start the fork",
    ))
    .await;
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    wait_until(
        || fixture.agent_result.lock().unwrap().is_some(),
        "the parent's Agent tool result",
    )
    .await;
}

fn open_db(data: &Path) -> rusqlite::Connection {
    let conn = rusqlite::Connection::open(data.join("deepseek_code.db")).unwrap();
    conn.busy_timeout(Duration::from_secs(10)).unwrap();
    conn
}

#[tokio::test]
async fn fork_runs_on_a_branch_of_the_parent_session() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(root.path().join("workspace")).unwrap();
    let fixture = Fixture::default();
    let (base, server) = fixture_server(fixture.clone()).await;
    write_providers(&data, &base);

    let session = uuid::Uuid::new_v4().to_string();
    run_parent_turns(&fixture, &data, root.path(), &session).await;
    wait_until(
        || !fixture.fork_requests.lock().unwrap().is_empty(),
        "the fork's first request",
    )
    .await;

    let child_request = serde_json::to_string(&fixture.fork_requests.lock().unwrap()[0]).unwrap();
    assert!(
        child_request.contains("PINEAPPLE-7"),
        "the fork did not receive the parent history: {child_request}"
    );
    assert!(
        child_request.contains(FORK_DIRECTIVE),
        "the fork directive is missing: {child_request}"
    );
    assert!(
        child_request.contains("say the marker"),
        "the prompt was not passed as the directive: {child_request}"
    );

    let result = fixture
        .agent_result
        .lock()
        .unwrap()
        .clone()
        .expect("Agent tool result");
    assert_eq!(result["backgrounded"], true, "{result}");
    let child = result["session_id"].as_str().expect("child session id");

    let conn = open_db(&data);
    let title: Option<String> = conn
        .query_row("SELECT title FROM conversations WHERE id=?1", [child], |row| {
            row.get(0)
        })
        .unwrap();
    assert!(
        title.as_deref().unwrap_or_default().starts_with("Fork: "),
        "unexpected fork title: {title:?}"
    );
    let setting: String = conn
        .query_row(
            "SELECT value FROM peripheral_settings WHERE key LIKE 'background-agent.%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let setting: Value = serde_json::from_str(&setting).unwrap();
    assert_eq!(setting["agent_type"], "fork", "{setting}");
    server.abort();
}

#[tokio::test]
async fn a_fork_cannot_start_another_fork() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(root.path().join("workspace")).unwrap();
    let fixture = Fixture {
        nested_fork: true,
        ..Default::default()
    };
    let (base, server) = fixture_server(fixture.clone()).await;
    write_providers(&data, &base);

    let session = uuid::Uuid::new_v4().to_string();
    run_parent_turns(&fixture, &data, root.path(), &session).await;
    wait_until(
        || fixture.nested_result.lock().unwrap().is_some(),
        "the nested fork tool result",
    )
    .await;

    let text = fixture
        .nested_result
        .lock()
        .unwrap()
        .clone()
        .expect("nested Agent tool result");
    assert!(
        text.contains("A fork cannot start another fork"),
        "nested fork was not refused: {text}"
    );
    server.abort();
}
