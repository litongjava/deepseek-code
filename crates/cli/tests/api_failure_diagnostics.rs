//! Real CLI runs against local fault services.
//!
//! Every service is a local socket under test control; no real credential,
//! billing endpoint or installed binary is used. The assertions compare the
//! final CLI error, the HTTP attempts the server actually received and the
//! session records persisted in SQLite, so the three cannot disagree silently.
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    process::Output,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    process::Command,
};

const API_KEY: &str = "sk-fixture-cli-key-0001";
const SESSION_MARKER: &str = "6a1f2c3d-0000-4000-8000-000000000001";

#[derive(Clone)]
enum Reply {
    /// Read the request, then close before a single response byte.
    CloseAfterRequest,
    /// Accept and never answer, so the request stays in flight.
    Hang,
    Status {
        code: u16,
        body: String,
        retry_after: Option<u64>,
    },
    /// First model round asks for a Bash tool call; later rounds emit partial
    /// text and then close, which must never be replayed.
    ToolThenPartialText,
    /// Emit partial text, end the chunked body cleanly and never send a
    /// completion marker: a stream that ends without completion.
    SsePartialCleanEnd,
}

struct Fixture {
    base: String,
    requests: Arc<AtomicUsize>,
}

async fn fixture(replies: Vec<Reply>) -> Fixture {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = requests.clone();
    tokio::spawn(async move {
        let mut index = 0usize;
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let reply = replies[index.min(replies.len() - 1)].clone();
            index += 1;
            counter.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let _ = answer(&mut socket, reply).await;
            });
        }
    });
    Fixture { base, requests }
}

impl Fixture {
    fn count(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    async fn wait_for_first_request(&self) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while self.count() == 0 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the CLI never reached the fixture"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

async fn answer(socket: &mut TcpStream, reply: Reply) -> std::io::Result<()> {
    match reply {
        Reply::Hang => {
            tokio::time::sleep(Duration::from_secs(60)).await;
            Ok(())
        }
        Reply::CloseAfterRequest => {
            read_request(socket).await?;
            Ok(())
        }
        Reply::Status {
            code,
            body,
            retry_after,
        } => {
            read_request(socket).await?;
            let retry = retry_after
                .map(|seconds| format!("retry-after: {seconds}\r\n"))
                .unwrap_or_default();
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 {code} Fixture\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n{retry}\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await?;
            socket.shutdown().await
        }
        Reply::SsePartialCleanEnd => {
            read_request(socket).await?;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
                )
                .await?;
            let payload =
                "data: {\"choices\":[{\"delta\":{\"content\":\"Partial clean\"},\"finish_reason\":null}]}\n\n";
            socket
                .write_all(format!("{:x}\r\n{payload}\r\n", payload.len()).as_bytes())
                .await?;
            socket.write_all(b"0\r\n\r\n").await?;
            socket.flush().await
        }
        Reply::ToolThenPartialText => {
            let request = read_request(socket).await?;
            let text = String::from_utf8_lossy(&request).to_ascii_lowercase();
            // A follow-up request carries the tool result, so it is round two.
            let round_two = text.contains("\"role\":\"tool\"");
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n",
                )
                .await?;
            if round_two {
                socket
                    .write_all(
                        b"data: {\"id\":\"r\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Partial text\"},\"finish_reason\":null}]}\n\n",
                    )
                    .await?;
            } else {
                let call = json!({
                    "id": "r", "model": "fixture",
                    "choices": [{
                        "index": 0,
                        "delta": {"tool_calls": [{
                            "index": 0, "id": "call-fixture", "type": "function",
                            "function": {"name": "Bash", "arguments": "{\"command\":\"printf a >> marker.txt\"}"}
                        }]},
                        "finish_reason": null
                    }]
                });
                let stop = json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":1,"completion_tokens":1}});
                socket
                    .write_all(
                        format!("data: {call}\n\ndata: {stop}\n\ndata: [DONE]\n\n").as_bytes(),
                    )
                    .await?;
            }
            socket.shutdown().await
        }
    }
}

async fn read_request(socket: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        let read = socket.read(&mut chunk).await?;
        if read == 0 {
            return Ok(buffer);
        }
        buffer.extend_from_slice(&chunk[..read]);
        let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let head = String::from_utf8_lossy(&buffer[..end]).to_ascii_lowercase();
        let length = head
            .lines()
            .find(|line| line.starts_with("content-length:"))
            .and_then(|line| line.split_once(':'))
            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        while buffer.len() < end + 4 + length {
            let read = socket.read(&mut chunk).await?;
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
        }
        return Ok(buffer);
    }
}

fn provider_config(base: &str) -> Value {
    json!([{
        "id": "fixture", "name": "Fixture", "base_url": base,
        "api_key": API_KEY, "api_format": "openai", "enabled": true,
        "models": [{
            "id": "fixture-model", "name": "Fixture", "enabled": true,
            "supports_vision": false, "supports_web_search": false, "max_tokens": 512
        }]
    }])
}

struct Project {
    temp: tempfile::TempDir,
}

impl Project {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("data")).unwrap();
        std::fs::create_dir_all(temp.path().join("project")).unwrap();
        std::fs::create_dir_all(temp.path().join("project/config")).unwrap();
        std::fs::write(temp.path().join("project/config/orchestration.toml"),
            include_str!("../../../src-tauri/config/orchestration.toml").replace("retry_base_backoff_ms = 2000", "retry_base_backoff_ms = 10")).unwrap();
        Self { temp }
    }

    fn data(&self) -> PathBuf {
        self.temp.path().join("data")
    }

    fn workspace(&self) -> PathBuf {
        self.temp.path().join("project")
    }

    fn configure(&self, base: &str) {
        std::fs::write(
            self.data().join("providers.json"),
            provider_config(base).to_string(),
        )
        .unwrap();
    }

    fn command(&self, session: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dcl"));
        command
            .current_dir(self.workspace())
            .args([
                "--data-dir",
                self.data().to_str().unwrap(),
                "--path",
                self.workspace().to_str().unwrap(),
                "--provider",
                "fixture",
                "--model",
                "fixture-model",
                "--session-id",
                session,
                "--permission-mode",
                "bypass",
                "-p",
                "hello",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        for key in [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "DEEPSEEK_API_KEY",
            "API_BASE_URL",
            "OPENAI_BASE_URL",
            "ANTHROPIC_BASE_URL",
            "CLAUDE_MODEL",
            "RUST_LOG",
            "DEEPSEEK_CODE_DATA_DIR",
            "DEEPSEEK_CODE_BACKGROUND_CONTROL",
            "CLAUDE_RUST_BACKGROUND_CONTROL",
            "DEEPSEEK_CODE_BACKGROUND_JOB",
            "CLAUDE_RUST_BACKGROUND_JOB",
            "DEEPSEEK_CODE_WORKER_MODEL",
            "CLAUDE_RUST_WORKER_MODEL",
            "DEEPSEEK_CODE_WORKER_AUTO_ROUTE",
            "CLAUDE_RUST_WORKER_AUTO_ROUTE",
        ] {
            command.env_remove(key);
        }
        command
    }
}

async fn execute(mut command: Command) -> Output {
    tokio::time::timeout(Duration::from_secs(60), command.output())
        .await
        .expect("the CLI did not exit")
        .unwrap()
}

/// Every assistant request snapshot persisted for the session.
fn persisted_snapshots(data: &Path, session: &str) -> Vec<Value> {
    let paths = deepseek_code_core::runtime::RuntimePaths::new(data.to_path_buf()).unwrap();
    let db = paths.open_database().unwrap();
    db.with_conn(|conn| -> anyhow::Result<Value> {
        let message: String = conn.query_row(
            "SELECT id FROM messages WHERE conversation_id=?1 AND role='assistant' ORDER BY sort_order DESC LIMIT 1",
            [session],
            |row| row.get(0),
        )?;
        deepseek_code_core::db::message_repo::get_request_snapshots(conn, &message)
    })
    .unwrap()
    .unwrap()
    .as_array()
    .cloned()
    .unwrap_or_default()
}

fn combined_output(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[tokio::test]
async fn attempts_are_consistent_between_cli_output_server_and_session() {
    let project = Project::new();
    let fixture = fixture(vec![Reply::CloseAfterRequest]).await;
    project.configure(&fixture.base);
    let output = execute(project.command(SESSION_MARKER)).await;
    assert!(!output.status.success(), "a failed turn must not exit zero");
    let text = combined_output(&output);

    let limit = deepseek_code_core::config::OrchestrationConfig::max_retries() + 1;
    assert_eq!(
        fixture.count(),
        limit as usize,
        "the server saw a different number of HTTP attempts: {text}"
    );
    for expected in [
        format!("attempts={limit}"),
        format!("retries={}", limit - 1),
        "stage=send".to_owned(),
    ] {
        assert!(text.contains(&expected), "missing {expected}: {text}");
    }
    assert!(!text.contains(API_KEY), "the API key leaked: {text}");

    let log = std::fs::read_to_string(project.data().join("logs/request-attempts").join(format!("{SESSION_MARKER}.jsonl"))).unwrap();
    let records: Vec<Value> = log.lines().map(|line| serde_json::from_str::<Value>(line).unwrap()).filter(|event| event["type"] == "request_attempt").collect();
    assert_eq!(records.len(), limit as usize);
    assert_eq!(records.last().unwrap()["outcome"], "failed");
    for (index, event) in records.iter().enumerate() {
        assert_eq!(event["attempts"], index + 1);
        assert_eq!(event["failure"]["stage"], "send");
    }
    assert!(text.contains("正在重连"));
    assert!(text.contains("第7次重连"));
    assert!(!log.contains(API_KEY));
    let snapshots = persisted_snapshots(&project.data(), SESSION_MARKER);
    let failure = snapshots
        .iter()
        .find_map(|snapshot| snapshot.get("failure").filter(|failure| !failure.is_null()))
        .unwrap_or_else(|| panic!("no structured failure recorded: {snapshots:?}"));
    assert_eq!(failure["attempts"], limit);
    assert_eq!(failure["retries"], limit - 1);
    assert_eq!(failure["stage"], "send");
    assert_eq!(failure["class"], "transient");
    let persisted_attempts = failure["attempts"].as_u64().unwrap() as usize;
    assert_eq!(persisted_attempts, fixture.count());
}

#[tokio::test]
async fn an_authentication_error_is_sent_once_and_keeps_its_meaning() {
    let project = Project::new();
    let fixture = fixture(vec![Reply::Status {
        code: 401,
        body: json!({"error": {"message": "invalid api key", "type": "authentication_error"}})
            .to_string(),
        retry_after: None,
    }])
    .await;
    project.configure(&fixture.base);
    let output = execute(project.command(SESSION_MARKER)).await;
    assert!(!output.status.success());
    assert_eq!(fixture.count(), 1, "authentication failures must not retry");
    let text = combined_output(&output);
    assert!(text.contains("401"), "{text}");
    assert!(text.contains("invalid api key"), "{text}");

    let snapshots = persisted_snapshots(&project.data(), SESSION_MARKER);
    let failure = snapshots
        .iter()
        .find_map(|snapshot| snapshot.get("failure").filter(|failure| !failure.is_null()))
        .expect("a structured failure is recorded");
    assert_eq!(failure["http"], 401);
    assert_eq!(failure["attempts"], 1);
    assert_eq!(failure["class"], "authentication");
}

/// The job directory `/tasks` creates for a worker; the CLI polls its `cancel`
/// file, which is exactly what `/tasks cancel ID` writes.
fn worker_directory(data: &Path, session: &str) -> PathBuf {
    let id = uuid::Uuid::new_v4().to_string();
    let directory = data.join("background-tasks").join(&id);
    std::fs::create_dir_all(&directory).unwrap();
    let job = json!({
        "id": id,
        "foreground": false,
        "parent_session_id": session,
        "session_id": session,
        "title": "fixture",
        "status": "running",
        "supervisor_pid": null,
        "child_pid": null,
        "started_ms": 1,
        "heartbeat_ms": chrono::Utc::now().timestamp_millis() as u64,
        "finished_ms": null,
        "exit_code": null,
        "error": null
    });
    std::fs::write(directory.join("state.json"), job.to_string()).unwrap();
    directory
}

async fn cancel_mid_request(reply: Reply, cancel_after: Duration) -> (usize, Vec<Value>, PathBuf) {
    let project = Project::new();
    let fixture = fixture(vec![reply]).await;
    project.configure(&fixture.base);
    let directory = worker_directory(&project.data(), SESSION_MARKER);
    let mut command = project.command(SESSION_MARKER);
    command.env(
        deepseek_code_core::services::background_sessions::CONTROL_ENV,
        &directory,
    );
    let mut child = command.spawn().unwrap();
    fixture.wait_for_first_request().await;
    tokio::time::sleep(cancel_after).await;
    std::fs::write(directory.join("cancel"), "").unwrap();
    let status = tokio::time::timeout(Duration::from_secs(30), child.wait())
        .await
        .expect("the CLI ignored the cancellation")
        .unwrap();
    assert!(!status.success(), "a cancelled turn must not exit zero");
    // Well past the next retry instant: nothing may be sent again.
    tokio::time::sleep(Duration::from_millis(
        deepseek_code_core::config::OrchestrationConfig::retry_base_backoff_ms() + 800,
    ))
    .await;
    let count = fixture.count();
    let snapshots = persisted_snapshots(&project.data(), SESSION_MARKER);
    (count, snapshots, project.data())
}

#[tokio::test]
async fn a_stream_that_ends_without_completion_reports_the_real_attempts() {
    let project = Project::new();
    let fixture = fixture(vec![Reply::CloseAfterRequest, Reply::SsePartialCleanEnd]).await;
    project.configure(&fixture.base);
    let output = execute(project.command(SESSION_MARKER)).await;
    assert!(!output.status.success(), "an incomplete stream is a failed turn");
    assert_eq!(fixture.count(), 2, "one disconnect, then the answering attempt");
    let text = combined_output(&output);
    for expected in ["attempts=2", "retries=1", "stage=stream"] {
        assert!(text.contains(expected), "missing {expected}: {text}");
    }

    let snapshots = persisted_snapshots(&project.data(), SESSION_MARKER);
    let failure = snapshots
        .iter()
        .find_map(|snapshot| snapshot.get("failure").filter(|failure| !failure.is_null()))
        .expect("the incomplete stream is recorded");
    assert_eq!(failure["attempts"], 2, "{failure}");
    assert_eq!(failure["retries"], 1, "{failure}");
    assert_eq!(failure["contentStarted"], true, "{failure}");
}

#[tokio::test]
async fn cancelling_while_waiting_for_a_response_sends_nothing_more() {
    let (count, snapshots, _) = cancel_mid_request(Reply::Hang, Duration::from_millis(300)).await;
    assert_eq!(count, 1, "a cancelled in-flight request was retried");
    assert!(
        snapshots
            .iter()
            .all(|snapshot| snapshot.get("failure").is_none_or(Value::is_null)),
        "cancellation must not record a request failure: {snapshots:?}"
    );
}

#[tokio::test]
async fn cancelling_during_backoff_sends_nothing_more() {
    let (count, _, _) = cancel_mid_request(
        Reply::Status {
            code: 503,
            body: json!({"error": {"message": "unavailable"}}).to_string(),
            retry_after: Some(30),
        },
        Duration::from_millis(400),
    )
    .await;
    assert_eq!(count, 1, "a cancelled retry delay was not respected");
}

#[tokio::test]
async fn a_completed_tool_is_not_rerun_after_a_stream_interruption() {
    let project = Project::new();
    let fixture = fixture(vec![Reply::ToolThenPartialText]).await;
    project.configure(&fixture.base);
    let output = execute(project.command(SESSION_MARKER)).await;
    let text = combined_output(&output);
    assert_eq!(
        fixture.count(),
        2,
        "the interrupted response was replayed: {text}"
    );
    let marker = project.workspace().join("marker.txt");
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap_or_default(),
        "a",
        "the completed tool ran more than once"
    );
    // The partial text is persisted once, not repeated by a replay.
    let paths = deepseek_code_core::runtime::RuntimePaths::new(project.data()).unwrap();
    let db = paths.open_database().unwrap();
    let content: String = db
        .with_conn(|conn| -> anyhow::Result<String> {
            Ok(conn.query_row(
                "SELECT content FROM messages WHERE conversation_id=?1 AND role='assistant' ORDER BY sort_order DESC LIMIT 1",
                [SESSION_MARKER],
                |row| row.get(0),
            )?)
        })
        .unwrap()
        .unwrap();
    assert_eq!(content.matches("Partial text").count(), 1, "{content}");

    let snapshots = persisted_snapshots(&project.data(), SESSION_MARKER);
    let failure = snapshots
        .iter()
        .find_map(|snapshot| snapshot.get("failure").filter(|failure| !failure.is_null()))
        .expect("the interruption is recorded");
    assert_eq!(failure["stage"], "stream");
    assert_eq!(failure["contentStarted"], true);
    // The send layer observed exactly one HTTP attempt, so the count is known
    // and must be reported instead of being replaced by a guess.
    assert_eq!(failure["attempts"], 1, "{failure}");
    assert_eq!(failure["retries"], 0, "{failure}");
}
