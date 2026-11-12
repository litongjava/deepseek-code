//! A real interactive pipe shell driven by the actual CLI in SDK mode: the
//! model loads `TaskInput` on demand, starts `Bash interaction:"pipe"`, writes
//! one UTF-8 message in the first turn, writes the next message in a second
//! turn of the same owning process, closes stdin, and reads the complete echo.
//!
//! Cross-process writing is proven separately by the core test
//! `cross_process_writer_feeds_an_owner_in_another_process`, because two
//! simultaneous model owners of one session are correctly rejected by session
//! ownership.
use axum::{extract::State, response::IntoResponse, routing::post, Json, Router};
use serde_json::{json, Value};
use std::{
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::Command,
};

#[derive(Clone, Default)]
struct Fixture {
    requests: Arc<Mutex<Vec<Value>>>,
    task_id: Arc<Mutex<Option<String>>>,
    inputs: Arc<Mutex<Vec<Value>>>,
    taskget: Arc<Mutex<Option<Value>>>,
    denied: Arc<Mutex<Option<String>>>,
    script: Arc<Mutex<Option<String>>>,
    last_tool_text: Arc<Mutex<Option<String>>>,
}

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

/// Tool results in order, with the call's name resolved from the assistant turn.
fn tool_history(body: &Value) -> Vec<(String, Value)> {
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    let mut names = std::collections::HashMap::new();
    for message in &messages {
        for call in message["tool_calls"].as_array().into_iter().flatten() {
            if let (Some(id), Some(name)) = (call["id"].as_str(), call["function"]["name"].as_str())
            {
                names.insert(id.to_owned(), name.to_owned());
            }
        }
    }
    messages
        .iter()
        .filter(|message| message["role"] == "tool")
        .filter_map(|message| {
            let name = names.get(message["tool_call_id"].as_str()?)?;
            let content = message["content"]
                .as_str()
                .and_then(|text| serde_json::from_str::<Value>(text).ok())
                .unwrap_or(Value::Null);
            Some((name.clone(), content))
        })
        .collect()
}

/// Number of plain user turns this process has been given (SDK input frames).
fn user_turns(body: &Value) -> usize {
    body["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|message| message["role"] == "user")
        .filter(|message| {
            message["content"].is_string()
                || message["content"]
                    .as_array()
                    .is_some_and(|blocks| blocks.iter().any(|b| b["text"].is_string()))
        })
        .count()
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
    let mut body: String = frames
        .into_iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect();
    body.push_str("data: [DONE]\n\n");
    ([("content-type", "text/event-stream")], body).into_response()
}

const FIRST: &str = "第一条消息 😀\n";
const SECOND: &str = "第二条消息 ✅\n";

async fn respond(
    State(fixture): State<Fixture>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    fixture.requests.lock().unwrap().push(body.clone());
    assert_eq!(body["stream"], true);
    let tools = tool_names(&body);
    let history = tool_history(&body);
    let loaded = tools.iter().any(|tool| tool == "TaskInput");
    let has_bash = history.iter().any(|(name, _)| name == "Bash");
    let inputs: Vec<Value> = history
        .iter()
        .filter(|(name, _)| name == "TaskInput")
        .map(|(_, value)| value.clone())
        .collect();
    let eof = inputs.iter().any(|value| value["eof"] == true);
    let turn = user_turns(&body);
    let framed = if !loaded {
        // Both TaskInput and TaskGet are deferred: load them before use.
        tool_call_frame(
            "call-search",
            "ToolSearch",
            &json!({"query":"select:TaskInput,TaskGet"}),
        )
    } else if !has_bash {
        tool_call_frame(
            "call-start",
            "Bash",
            &json!({
                "command":"cat",
                "description":"Interactive echo shell",
                "run_in_background":true,
                "interaction":"pipe"
            }),
        )
    } else {
        let id = history
            .iter()
            .find(|(name, _)| name == "Bash")
            .and_then(|(_, value)| value["task_id"].as_str().map(str::to_owned))
            .unwrap_or_else(|| panic!("Bash result has no task_id: {body}"));
        *fixture.task_id.lock().unwrap() = Some(id.clone());
        *fixture.inputs.lock().unwrap() = inputs.clone();
        if inputs.is_empty() {
            tool_call_frame(
                "call-first",
                "TaskInput",
                &json!({"task_id":id,"data":FIRST}),
            )
        } else if !eof {
            if turn < 2 {
                // The first turn ends after its single message.
                text_frames("first turn complete")
            } else if inputs.len() == 1 {
                tool_call_frame(
                    "call-second",
                    "TaskInput",
                    &json!({"task_id":id,"data":SECOND}),
                )
            } else {
                tool_call_frame("call-eof", "TaskInput", &json!({"task_id":id,"eof":true}))
            }
        } else if let Some((_, got)) = history.iter().find(|(name, _)| name == "TaskGet") {
            *fixture.taskget.lock().unwrap() = Some(got.clone());
            text_frames("pipe shell observed")
        } else {
            tool_call_frame(
                "call-get",
                "TaskGet",
                &json!({"task_id":id,"block":true,"timeout":15000}),
            )
        }
    };
    stream(framed)
}

async fn start_server() -> (String, Fixture) {
    start_server_with(post(respond)).await
}

async fn start_server_with(responder: axum::routing::MethodRouter<Fixture>) -> (String, Fixture) {
    let fixture = Fixture::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/chat/completions", post(responder))
        .with_state(fixture.clone());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (base, fixture)
}

fn provider_config(base: &str) -> Value {
    json!([{
        "id":"fixture","name":"Fixture","base_url":base,"api_key":"fixture-only",
        "api_format":"openai","enabled":true,
        "models":[{"id":"fixture-model","name":"Fixture","enabled":true,
            "context_window":128000,"max_tokens":4096,
            "supports_web_search":false,"supports_vision":false}]
    }])
}

struct StreamClient {
    child: tokio::process::Child,
    stdin: Option<tokio::process::ChildStdin>,
    lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
}

impl StreamClient {
    fn start(data: &std::path::Path, root: &std::path::Path, session: &str) -> Self {
        Self::start_with(data, root, session, "bypassPermissions")
    }

    fn start_with(
        data: &std::path::Path,
        root: &std::path::Path,
        session: &str,
        mode: &str,
    ) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dcl"));
        command
            .args([
                "--data-dir",
                data.to_str().unwrap(),
                "--path",
                root.to_str().unwrap(),
                "--provider",
                "fixture",
                "--model",
                "fixture-model",
                "--permission-mode",
                mode,
                "--session-id",
                session,
                "--print",
                "--input-format",
                "stream-json",
                "--output-format",
                "stream-json",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        for (key, _) in std::env::vars() {
            if key.starts_with("DEEPSEEK_CODE") {
                command.env_remove(key);
            }
        }
        let mut child = command.spawn().unwrap();
        let stdin = child.stdin.take();
        let lines = BufReader::new(child.stdout.take().unwrap()).lines();
        Self {
            child,
            stdin,
            lines,
        }
    }

    async fn send(&mut self, value: Value) {
        let stdin = self.stdin.as_mut().unwrap();
        stdin
            .write_all(format!("{value}\n").as_bytes())
            .await
            .unwrap();
        stdin.flush().await.unwrap();
    }

    async fn result(&mut self) -> Value {
        tokio::time::timeout(Duration::from_secs(90), async {
            loop {
                let line = self
                    .lines
                    .next_line()
                    .await
                    .unwrap()
                    .expect("CLI ended early");
                let frame: Value = serde_json::from_str(&line).unwrap();
                if frame["type"] == "result" {
                    return frame;
                }
            }
        })
        .await
        .expect("CLI did not finish the turn")
    }

    async fn finish(mut self) -> std::process::ExitStatus {
        self.stdin.take();
        tokio::time::timeout(Duration::from_secs(60), self.child.wait())
            .await
            .expect("CLI did not exit")
            .unwrap()
    }
}

fn user(text: &str) -> Value {
    json!({"type":"user","message":{"role":"user","content":text}})
}

#[tokio::test]
async fn cli_pipe_shell_takes_utf8_input_across_two_turns() {
    let (base, fixture) = start_server().await;
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(
        data.join("providers.json"),
        provider_config(&base).to_string(),
    )
    .unwrap();
    let session = uuid::Uuid::new_v4().to_string();

    let mut owner = StreamClient::start(&data, root.path(), &session);
    owner.send(user("start the interactive shell")).await;
    let first = owner.result().await;
    assert_eq!(first["is_error"], false, "{first}");

    let task_id = fixture
        .task_id
        .lock()
        .unwrap()
        .clone()
        .expect("the Bash result never carried a task id");
    let requests = fixture.requests.lock().unwrap().clone();
    assert!(
        !tool_names(&requests[0])
            .iter()
            .any(|tool| tool == "TaskInput"),
        "TaskInput must load on demand: {}",
        tool_names(&requests[0]).join(",")
    );
    assert!(
        requests.iter().any(|request| tool_names(request)
            .iter()
            .filter(|tool| *tool == "TaskInput")
            .count()
            == 1),
        "TaskInput was never advertised exactly once"
    );
    let inputs = fixture.inputs.lock().unwrap().clone();
    assert_eq!(inputs.len(), 1, "{inputs:?}");
    assert_eq!(inputs[0]["task_id"], task_id, "{inputs:?}");
    assert_eq!(inputs[0]["bytes_written"], FIRST.len(), "{inputs:?}");
    assert_eq!(inputs[0]["eof"], false, "{inputs:?}");

    // The second turn runs in the same owning process and writes again.
    owner.send(user("continue the same shell")).await;
    let second = owner.result().await;
    assert_eq!(second["is_error"], false, "{second}");
    let status = owner.finish().await;
    assert!(status.success(), "{status}");

    let observed = fixture
        .taskget
        .lock()
        .unwrap()
        .clone()
        .expect("TaskGet never returned the pipe shell");
    assert_eq!(observed["status"], "completed", "{observed}");
    assert_eq!(observed["retrieval_status"], "success", "{observed}");
    assert_eq!(
        observed["stdout"],
        format!("{FIRST}{SECOND}"),
        "the UTF-8 input was not echoed byte for byte: {observed}"
    );
    let inputs = fixture.inputs.lock().unwrap().clone();
    assert_eq!(inputs.last().unwrap()["eof"], true, "{inputs:?}");
    let second_input = inputs
        .iter()
        .find(|value| value["bytes_written"] == SECOND.len())
        .unwrap_or_else(|| panic!("the second message was never written: {inputs:?}"));
    assert_eq!(
        second_input["bytes_written"],
        SECOND.len(),
        "{second_input}"
    );
}

/// Plan mode must refuse TaskInput while still allowing the read-only part of
/// the flow (ToolSearch and an ordinary shell command).
async fn plan_respond(
    State(fixture): State<Fixture>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    fixture.requests.lock().unwrap().push(body.clone());
    let tools = tool_names(&body);
    let history = tool_history(&body);
    let messages = body["messages"].as_array().cloned().unwrap_or_default();
    let last_tool_text = messages
        .iter()
        .rev()
        .find(|message| message["role"] == "tool")
        .and_then(|message| message["content"].as_str())
        .unwrap_or_default()
        .to_owned();
    fixture
        .denied
        .lock()
        .unwrap()
        .replace(last_tool_text.clone());
    let loaded = tools.iter().any(|tool| tool == "TaskInput");
    let has_bash = history.iter().any(|(name, _)| name == "Bash");
    let attempted = history.iter().any(|(name, _)| name == "TaskInput");
    let stopped = history.iter().any(|(name, _)| name == "TaskStop");
    let framed = if !loaded {
        tool_call_frame(
            "plan-search",
            "ToolSearch",
            &json!({"query":"select:TaskInput"}),
        )
    } else if !has_bash {
        tool_call_frame(
            "plan-start",
            "Bash",
            &json!({
                "command":"cat",
                "description":"Interactive echo shell",
                "run_in_background":true,
                "interaction":"pipe"
            }),
        )
    } else if !attempted {
        let id = history
            .iter()
            .find(|(name, _)| name == "Bash")
            .and_then(|(_, value)| value["task_id"].as_str().map(str::to_owned))
            .unwrap();
        fixture.task_id.lock().unwrap().replace(id.clone());
        tool_call_frame(
            "plan-input",
            "TaskInput",
            &json!({"task_id":id,"data":"forbidden"}),
        )
    } else if !stopped {
        let id = fixture.task_id.lock().unwrap().clone().unwrap();
        tool_call_frame("plan-stop", "TaskStop", &json!({"task_id":id}))
    } else {
        text_frames("plan mode refused the input")
    };
    stream(framed)
}

#[tokio::test]
async fn cli_task_input_is_refused_in_plan_mode() {
    let (base, fixture) = start_server_with(post(plan_respond)).await;
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(
        data.join("providers.json"),
        provider_config(&base).to_string(),
    )
    .unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    let mut owner = StreamClient::start_with(&data, root.path(), &session, "plan");
    owner.send(user("start an interactive shell")).await;
    let result = owner.result().await;
    assert_eq!(result["is_error"], false, "{result}");
    let denied = fixture.denied.lock().unwrap().clone().unwrap_or_default();
    assert!(
        denied.contains("Plan mode") && !denied.contains("\"bytes_written\""),
        "TaskInput was not refused by the permission layer: {denied}"
    );
    let status = owner.finish().await;
    assert!(status.success(), "{status}");
}

/// A graceful owner shutdown (the SDK session ends and the process exits) must
/// take the interactive pipe shell and a descendant that never reads stdin with
/// it. Only the exact recorded PIDs are inspected.
#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .is_ok_and(|status| status.success())
    }
}

#[cfg(unix)]
fn wait_for_pid_file(path: &std::path::Path, timeout: Duration) -> u32 {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(pid) = text.trim().parse::<u32>() {
                return pid;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no PID at {} within {timeout:?}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

async fn shutdown_respond(
    State(fixture): State<Fixture>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    fixture.requests.lock().unwrap().push(body.clone());
    let history = tool_history(&body);
    let framed = if history.iter().any(|(name, _)| name == "Bash") {
        text_frames("the shell is running")
    } else {
        let script = fixture
            .script
            .lock()
            .unwrap()
            .clone()
            .expect("the test never provided a shell script");
        tool_call_frame(
            "shutdown-start",
            "Bash",
            &json!({
                "command":format!("sh {script}"),
                "description":"Shell that never reads stdin",
                "run_in_background":true,
                "interaction":"pipe"
            }),
        )
    };
    stream(framed)
}

#[cfg(unix)]
#[tokio::test]
async fn cli_graceful_shutdown_stops_the_pipe_shell_and_its_descendant() {
    let (base, fixture) = start_server_with(post(shutdown_respond)).await;
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(
        data.join("providers.json"),
        provider_config(&base).to_string(),
    )
    .unwrap();
    let shell_pid = root.path().join("root.pid");
    let descendant_pid = root.path().join("descendant.pid");
    let script = root.path().join("shell.sh");
    std::fs::write(
        &script,
        format!(
            "echo $$ > '{}'\nsh -c 'echo $$ > \"{}\"; while :; do sleep 1; done' &\nwhile :; do sleep 1; done\n",
            shell_pid.display(),
            descendant_pid.display()
        ),
    )
    .unwrap();
    *fixture.script.lock().unwrap() = Some(script.display().to_string());
    let session = uuid::Uuid::new_v4().to_string();
    let mut owner = StreamClient::start(&data, root.path(), &session);
    owner.send(user("start a shell that ignores stdin")).await;
    let result = owner.result().await;
    assert_eq!(result["is_error"], false, "{result}");

    let shell = wait_for_pid_file(&shell_pid, Duration::from_secs(20));
    let descendant = wait_for_pid_file(&descendant_pid, Duration::from_secs(20));
    assert!(process_alive(shell), "the shell never started");
    assert!(process_alive(descendant), "the descendant never started");

    // Graceful shutdown: stdin closes, the turn loop ends and the process exits.
    let status = owner.finish().await;
    assert!(status.success(), "{status}");

    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while process_alive(shell) || process_alive(descendant) {
        assert!(
            std::time::Instant::now() < deadline,
            "graceful shutdown left the shell {shell} or descendant {descendant} running"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// ---------------------------------------------------------------------------
// Round 20: per-call records and hooks through the ordinary tool pipeline.
// ---------------------------------------------------------------------------

/// The raw tool message answering the newest call of `name`.
fn raw_tool_message(body: &Value, name: &str) -> Option<String> {
    let messages = body["messages"].as_array()?;
    let mut ids = Vec::new();
    for message in messages {
        for call in message["tool_calls"].as_array().into_iter().flatten() {
            if call["function"]["name"].as_str() == Some(name) {
                if let Some(id) = call["id"].as_str() {
                    ids.push(id.to_owned());
                }
            }
        }
    }
    messages.iter().rev().find_map(|message| {
        let id = message["tool_call_id"].as_str()?;
        if message["role"] == "tool" && ids.iter().any(|candidate| candidate == id) {
            message["content"].as_str().map(str::to_owned)
        } else {
            None
        }
    })
}

fn hook_journal(path: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn write_hooks(workspace: &std::path::Path, settings: &Value) {
    let directory = workspace.join(".claude");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("settings.json"), settings.to_string()).unwrap();
}

#[derive(Debug)]
struct StoredCall {
    id: String,
    message_id: String,
    input: String,
    output: String,
    is_error: i64,
}

fn tool_call_rows(data: &std::path::Path, session: &str) -> Vec<StoredCall> {
    let conn = rusqlite::Connection::open(data.join("deepseek_code.db")).unwrap();
    conn.busy_timeout(std::time::Duration::from_secs(10))
        .unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT tc.id, tc.message_id, tc.input, tc.output, tc.is_error FROM tool_calls tc \
             JOIN messages m ON m.id = tc.message_id \
             WHERE m.conversation_id = ?1 AND tc.name = 'TaskInput' \
             ORDER BY m.sort_order, tc.sort_order",
        )
        .unwrap();
    stmt.query_map([session], |row| {
        Ok(StoredCall {
            id: row.get(0)?,
            message_id: row.get(1)?,
            input: row.get(2)?,
            output: row.get(3)?,
            is_error: row.get(4)?,
        })
    })
    .unwrap()
    .collect::<Result<Vec<_>, _>>()
    .unwrap()
}

const RECORDS_FIRST: &str = "记录一 ✅\n";
const RECORDS_SECOND: &str = "第二条记录明显更长 😀✅\n";

/// Turn 1 writes one message; turn 2 writes the next and closes stdin. Both
/// TaskInput calls carry the same provider tool-call id, so the durable rows and
/// hook ids must still be per execution.
async fn records_respond(
    State(fixture): State<Fixture>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    fixture.requests.lock().unwrap().push(body.clone());
    assert_eq!(body["stream"], true);
    let tools = tool_names(&body);
    let history = tool_history(&body);
    let inputs: Vec<Value> = history
        .iter()
        .filter(|(name, _)| name == "TaskInput")
        .map(|(_, value)| value.clone())
        .collect();
    let loaded = tools.iter().any(|tool| tool == "TaskInput");
    let has_bash = history.iter().any(|(name, _)| name == "Bash");
    let has_get = history.iter().any(|(name, _)| name == "TaskGet");
    let has_eof = inputs.iter().any(|value| value["eof"] == true);
    let turn = user_turns(&body);
    let framed = if !loaded {
        tool_call_frame(
            "records-search",
            "ToolSearch",
            &json!({"query":"select:TaskInput,TaskGet"}),
        )
    } else if !has_bash {
        tool_call_frame(
            "records-start",
            "Bash",
            &json!({
                "command":"cat",
                "description":"Recorded interactive shell",
                "run_in_background":true,
                "interaction":"pipe"
            }),
        )
    } else {
        let id = history
            .iter()
            .find(|(name, _)| name == "Bash")
            .and_then(|(_, value)| value["task_id"].as_str().map(str::to_owned))
            .unwrap_or_else(|| panic!("Bash result has no task_id: {body}"));
        *fixture.task_id.lock().unwrap() = Some(id.clone());
        *fixture.inputs.lock().unwrap() = inputs.clone();
        if inputs.is_empty() {
            // Reused on purpose: the same provider raw id appears again below.
            tool_call_frame(
                "reused-provider",
                "TaskInput",
                &json!({"task_id":id,"data":RECORDS_FIRST}),
            )
        } else if turn < 2 {
            text_frames("first turn recorded")
        } else if inputs.len() == 1 {
            tool_call_frame(
                "reused-provider",
                "TaskInput",
                &json!({"task_id":id,"data":RECORDS_SECOND}),
            )
        } else if !has_eof {
            tool_call_frame(
                "records-eof",
                "TaskInput",
                &json!({"task_id":id,"eof":true}),
            )
        } else if !has_get {
            tool_call_frame(
                "records-get",
                "TaskGet",
                &json!({"task_id":id,"block":true,"timeout":15000}),
            )
        } else {
            *fixture.taskget.lock().unwrap() = history
                .iter()
                .find(|(name, _)| name == "TaskGet")
                .map(|(_, value)| value.clone());
            text_frames("records complete")
        }
    };
    stream(framed)
}

#[tokio::test]
async fn cli_task_input_records_each_call_and_fires_tool_hooks() {
    let (base, fixture) = start_server_with(post(records_respond)).await;
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(
        data.join("providers.json"),
        provider_config(&base).to_string(),
    )
    .unwrap();
    let settings = json!({"hooks":{
        "PreToolUse":[{"matcher":"TaskInput","hooks":[{"type":"command","shell":"sh","command":"cat >> hooks.jsonl"}]}],
        "PostToolUse":[{"matcher":"TaskInput","hooks":[{"type":"command","shell":"sh","command":"cat >> hooks.jsonl"}]}]
    }});
    write_hooks(root.path(), &settings);
    let session = uuid::Uuid::new_v4().to_string();

    let mut owner = StreamClient::start(&data, root.path(), &session);
    owner.send(user("start the recorded shell")).await;
    let first = owner.result().await;
    assert_eq!(first["is_error"], false, "{first}");
    owner.send(user("write the second message")).await;
    let second = owner.result().await;
    assert_eq!(second["is_error"], false, "{second}");
    let status = owner.finish().await;
    assert!(status.success(), "{status}");

    // The provider raw id really was reused across two assistant messages.
    let requests = fixture.requests.lock().unwrap().clone();
    let reused = requests
        .last()
        .unwrap()
        .to_string()
        .matches("\"reused-provider\"")
        .count();
    assert!(
        reused >= 2,
        "the provider tool-call id was not reused in the request history: {reused}"
    );

    let id = fixture
        .task_id
        .lock()
        .unwrap()
        .clone()
        .expect("the Bash result never carried a task id");
    // Durable rows: one per execution, keyed by assistant message + occurrence.
    let rows = tool_call_rows(&data, &session);
    assert_eq!(
        rows.len(),
        3,
        "expected three durable TaskInput rows: {rows:?}"
    );
    let mut ids = std::collections::BTreeSet::new();
    let mut occurrences = std::collections::BTreeSet::new();
    let mut inputs = Vec::new();
    let mut outputs = Vec::new();
    for row in &rows {
        assert!(
            row.id.starts_with(&format!("{}:inv-", row.message_id)),
            "{}",
            row.id
        );
        assert!(
            ids.insert(row.id.clone()),
            "duplicate durable id {}",
            row.id
        );
        let occurrence = row.id.rsplit_once(':').unwrap().1.to_owned();
        assert!(occurrence.starts_with("inv-"), "{}", row.id);
        assert!(
            occurrences.insert(occurrence),
            "duplicate occurrence {}",
            row.id
        );
        assert_eq!(
            row.is_error, 0,
            "a successful call was recorded as an error"
        );
        inputs.push(serde_json::from_str::<Value>(&row.input).unwrap());
        outputs.push(serde_json::from_str::<Value>(&row.output).unwrap());
    }
    assert_eq!(inputs[0]["data"], RECORDS_FIRST, "{inputs:?}");
    assert_eq!(inputs[1]["data"], RECORDS_SECOND, "{inputs:?}");
    assert_eq!(inputs[2]["eof"], true, "{inputs:?}");
    // The first row was not overwritten by the later call with the same raw id.
    assert_ne!(inputs[0]["data"], inputs[1]["data"]);
    // Each persisted result belongs to its own call: the byte count matches that
    // call's UTF-8 data, and only the EOF call closed stdin.
    assert_eq!(
        outputs[0]["bytes_written"],
        RECORDS_FIRST.len(),
        "{outputs:?}"
    );
    assert_eq!(
        outputs[1]["bytes_written"],
        RECORDS_SECOND.len(),
        "{outputs:?}"
    );
    assert_eq!(outputs[0]["eof"], false, "{outputs:?}");
    assert_eq!(outputs[1]["eof"], false, "{outputs:?}");
    assert_eq!(outputs[0]["stdin_closed"], false, "{outputs:?}");
    assert_eq!(outputs[2]["eof"], true, "{outputs:?}");
    assert_eq!(outputs[2]["stdin_closed"], true, "{outputs:?}");
    assert_eq!(outputs[2]["bytes_written"], 0, "{outputs:?}");
    for output in &outputs {
        assert_eq!(
            output["task_id"], id,
            "results refer to another task: {outputs:?}"
        );
        assert_eq!(output["input_mode"], "pipe", "{outputs:?}");
        assert_eq!(output["write_complete"], true, "{outputs:?}");
    }

    // Hooks fire once per real execution, with the internal occurrence id that
    // the durable row also uses.
    let journal = hook_journal(&root.path().join("hooks.jsonl"));
    let pre: Vec<&Value> = journal
        .iter()
        .filter(|event| event["hook_event_name"] == "PreToolUse")
        .collect();
    let post: Vec<&Value> = journal
        .iter()
        .filter(|event| event["hook_event_name"] == "PostToolUse")
        .collect();
    assert_eq!(pre.len(), 3, "{journal:?}");
    assert_eq!(post.len(), 3, "{journal:?}");
    let pre_ids = pre
        .iter()
        .map(|event| event["tool_use_id"].as_str().unwrap().to_owned())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(pre_ids.len(), 3, "hook ids repeated: {pre_ids:?}");
    for id in &pre_ids {
        assert!(
            id.starts_with("inv-"),
            "the hook saw a provider id instead of the occurrence id: {id}"
        );
        assert!(!id.contains("reused-provider"), "{id}");
    }
    let stored_occurrences: std::collections::BTreeSet<String> = occurrences.into_iter().collect();
    assert_eq!(
        pre_ids, stored_occurrences,
        "hook ids and durable occurrence ids disagree"
    );
    let post_ids = post
        .iter()
        .map(|event| event["tool_use_id"].as_str().unwrap().to_owned())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(post_ids, pre_ids, "PostToolUse correlation is incomplete");
    assert_eq!(pre[0]["tool_name"], "TaskInput", "{:?}", pre[0]);
    // PostToolUse carries the matching response fields per occurrence.
    let expected_bytes = [
        ("data", RECORDS_FIRST.len()),
        ("data", RECORDS_SECOND.len()),
        ("eof", 0),
    ];
    for (event, (kind, bytes)) in pre.iter().zip(expected_bytes) {
        let occurrence = event["tool_use_id"].as_str().unwrap();
        let paired = post
            .iter()
            .find(|candidate| candidate["tool_use_id"] == occurrence)
            .unwrap_or_else(|| panic!("PostToolUse missing for {occurrence}"));
        let response = paired["tool_response"]
            .as_str()
            .unwrap_or_else(|| panic!("PostToolUse has no response: {paired}"));
        let response: Value = serde_json::from_str(response).unwrap();
        assert_eq!(response["bytes_written"], bytes, "{paired}");
        assert_eq!(response["task_id"], id, "{paired}");
        if kind == "eof" {
            assert_eq!(response["eof"], true, "{paired}");
        } else {
            assert_eq!(response["eof"], false, "{paired}");
        }
    }

    let observed = fixture
        .taskget
        .lock()
        .unwrap()
        .clone()
        .expect("TaskGet never returned the recorded shell");
    assert_eq!(observed["status"], "completed", "{observed}");
    assert_eq!(
        observed["stdout"],
        format!("{RECORDS_FIRST}{RECORDS_SECOND}"),
        "{observed}"
    );
}

/// A PreToolUse denial must stop the call before any stdin byte exists.
async fn deny_respond(
    State(fixture): State<Fixture>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    fixture.requests.lock().unwrap().push(body.clone());
    let tools = tool_names(&body);
    let history = tool_history(&body);
    let has_bash = history.iter().any(|(name, _)| name == "Bash");
    let has_input = history.iter().any(|(name, _)| name == "TaskInput");
    let has_stop = history.iter().any(|(name, _)| name == "TaskStop");
    let has_get = history.iter().any(|(name, _)| name == "TaskGet");
    if let Some(text) = raw_tool_message(&body, "TaskInput") {
        *fixture.last_tool_text.lock().unwrap() = Some(text);
    }
    let framed = if !tools.iter().any(|tool| tool == "TaskInput") {
        tool_call_frame(
            "deny-search",
            "ToolSearch",
            &json!({"query":"select:TaskInput,TaskGet"}),
        )
    } else if !has_bash {
        tool_call_frame(
            "deny-start",
            "Bash",
            &json!({
                "command":"cat",
                "description":"Denied interactive shell",
                "run_in_background":true,
                "interaction":"pipe"
            }),
        )
    } else {
        let id = history
            .iter()
            .find(|(name, _)| name == "Bash")
            .and_then(|(_, value)| value["task_id"].as_str().map(str::to_owned))
            .unwrap_or_else(|| panic!("Bash result has no task_id: {body}"));
        *fixture.task_id.lock().unwrap() = Some(id.clone());
        if !has_input {
            tool_call_frame(
                "deny-input",
                "TaskInput",
                &json!({"task_id":id,"data":"must-not-arrive"}),
            )
        } else if !has_stop {
            tool_call_frame("deny-stop", "TaskStop", &json!({"task_id":id}))
        } else if !has_get {
            tool_call_frame("deny-get", "TaskGet", &json!({"task_id":id}))
        } else {
            *fixture.taskget.lock().unwrap() = history
                .iter()
                .find(|(name, _)| name == "TaskGet")
                .map(|(_, value)| value.clone());
            text_frames("denied call observed")
        }
    };
    stream(framed)
}

#[tokio::test]
async fn cli_task_input_pre_hook_denial_writes_no_stdin_bytes() {
    let (base, fixture) = start_server_with(post(deny_respond)).await;
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(
        data.join("providers.json"),
        provider_config(&base).to_string(),
    )
    .unwrap();
    write_hooks(
        root.path(),
        &json!({"hooks":{
            "PreToolUse":[{"matcher":"TaskInput","hooks":[{
                "type":"command","shell":"sh",
                "command":"payload=$(cat); printf '%s\\n' \"$payload\" >> holes.jsonl; printf '%s' '{\"hookSpecificOutput\":{\"hookEventName\":\"PreToolUse\",\"permissionDecision\":\"deny\",\"permissionDecisionReason\":\"FIXTURE_DENIED\"}}'"
            }]}]
        }}),
    );
    let session = uuid::Uuid::new_v4().to_string();
    let mut owner = StreamClient::start(&data, root.path(), &session);
    owner.send(user("start the denied shell")).await;
    let result = owner.result().await;
    assert_eq!(result["is_error"], false, "{result}");
    let status = owner.finish().await;
    assert!(status.success(), "{status}");

    let journal = hook_journal(&root.path().join("holes.jsonl"));
    assert_eq!(journal.len(), 1, "{journal:?}");
    assert_eq!(journal[0]["hook_event_name"], "PreToolUse", "{journal:?}");
    assert_eq!(journal[0]["tool_name"], "TaskInput", "{journal:?}");
    assert_eq!(
        journal[0]["tool_input"]["data"], "must-not-arrive",
        "{journal:?}"
    );

    // The tool result the model saw is the denial, recorded as an error.
    let denied = fixture
        .last_tool_text
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_default();
    assert!(
        denied.contains("FIXTURE_DENIED"),
        "the model did not receive the denial: {denied}"
    );
    assert!(
        !denied.contains("bytes_written"),
        "a denied call still reported a write: {denied}"
    );
    let rows = tool_call_rows(&data, &session);
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(
        rows[0].is_error, 1,
        "the denied call was recorded as success"
    );

    // `cat` echoes everything it receives: an empty log proves zero bytes
    // reached its stdin.
    let observed = fixture
        .taskget
        .lock()
        .unwrap()
        .clone()
        .expect("TaskGet never returned the denied shell");
    assert_eq!(observed["stdout"], "", "{observed}");
}
