//! Real CLI runs against local fault services.
//!
//! Every service is a local socket under test control; no real credential,
//! billing endpoint or installed binary is used. The assertions compare the
//! final CLI error, the HTTP attempts the server actually received and the
//! session records persisted in SQLite, so the three cannot disagree silently.
#[path = "../../core/tests/support/session_sql.rs"]
mod fixture_sql;
use fixture_sql::FixtureSql;
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
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    process::Command,
};

const API_KEY: &str = "sk-fixture-cli-key-0001";
/// Free text a server could echo; it must never reach a diagnostic.
const MARKER: &str = "PRIVATE_REVIEW4_PROMPT";
const SESSION_MARKER: &str = "6a1f2c3d-0000-4000-8000-000000000001";
/// Fake provider text that echoes input; it must never reach diagnostics.
const ECHO_MARKER: &str = "PRIVATE_PROVIDER_ECHO_0b7c";

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
    /// The same, in the Anthropic Messages stream format.
    AnthropicPartialCleanEnd,
    /// HTTP 200 with an SSE error event carrying the marker.
    SseError { anthropic: bool },
    /// 429 with a recognized permanent quota identifier.
    QuotaError,
    /// Built-in Chat Completions: a known empty prelude, then a clean EOF.
    BuiltinEmptyThenClose,
    /// Built-in Chat Completions: reasoning content, then a clean EOF.
    BuiltinReasoningThenClose,
    /// Built-in Chat Completions: a complete tool call with no completion marker.
    BuiltinToolThenClose,
    /// Built-in Chat Completions: a complete text response.
    BuiltinComplete,
    /// Built-in Chat Completions: an SSE provider error echoing a unique marker.
    BuiltinProviderErrorEcho,
    /// A scripted SSE body, written chunk by chunk.
    Script { steps: Vec<Step> },
    /// The same, but the request body is captured for later comparison.
    RecordScript {
        steps: Vec<Step>,
        bodies: Arc<std::sync::Mutex<Vec<Vec<u8>>>>,
        /// When each request head was read, so a POST can be ordered against a
        /// frame the test observed on stdout.
        posts: Arc<std::sync::Mutex<Vec<std::time::Instant>>>,
    },
    /// Flushed HTTP 200 headers and then no body bytes at all.
    HeadersThenSilence,
}

/// One step of a scripted SSE response.
#[derive(Clone)]
enum Step {
    Write(&'static str),
    Pause(u64),
    /// Everything written so far has flushed; the test is told before the hold.
    Signal(Arc<tokio::sync::Notify>),
    /// Keep the connection open and send nothing else.
    Hold,
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

/// Write one scripted SSE body: each step is a chunk, then a clean end unless a
/// step holds the body open.
async fn write_script(socket: &mut TcpStream, steps: Vec<Step>) -> std::io::Result<()> {
    socket
        .write_all(
            b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
        )
        .await?;
    socket.flush().await?;
    for step in steps {
        match step {
            Step::Write(payload) => {
                socket
                    .write_all(format!("{:x}\r\n{payload}\r\n", payload.len()).as_bytes())
                    .await?;
                socket.flush().await?;
            }
            Step::Pause(millis) => tokio::time::sleep(Duration::from_millis(millis)).await,
            Step::Signal(notify) => notify.notify_one(),
            Step::Hold => {
                tokio::time::sleep(Duration::from_secs(600)).await;
                return Ok(());
            }
        }
    }
    socket.write_all(b"0\r\n\r\n").await?;
    socket.flush().await
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
        Reply::AnthropicPartialCleanEnd => {
            read_request(socket).await?;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
                )
                .await?;
            let payload = "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Partial anthro\"}}\n\n";
            socket
                .write_all(format!("{:x}\r\n{payload}\r\n", payload.len()).as_bytes())
                .await?;
            socket.write_all(b"0\r\n\r\n").await?;
            socket.flush().await
        }

        Reply::SseError { anthropic } => {
            read_request(socket).await?;
            // Both protocols carry the same error shape; the label only keeps
            // the two cases readable in the test name.
            let event = json!({
                "type": "error",
                "error": {"type": "invalid_request_error", "message": MARKER}
            });
            let _ = anthropic;
            let header = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n";
            socket.write_all(header.as_bytes()).await?;
            let payload = format!("data: {event}\n\n");
            socket
                .write_all(format!("{:x}\r\n{payload}\r\n", payload.len()).as_bytes())
                .await?;
            socket.write_all(b"0\r\n\r\n").await?;
            socket.flush().await
        }
        Reply::QuotaError => {
            read_request(socket).await?;
            let body = json!({
                "error": {
                    "message": "insufficient quota",
                    "code": "insufficient_quota",
                    "type": "insufficient_quota"
                }
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await?;
            socket.shutdown().await
        }
        Reply::BuiltinEmptyThenClose
        | Reply::BuiltinReasoningThenClose
        | Reply::BuiltinToolThenClose
        | Reply::BuiltinProviderErrorEcho
        | Reply::BuiltinComplete => {
            read_request(socket).await?;
            let body = match reply {
                Reply::BuiltinEmptyThenClose => {
                    "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n"
                        .to_string()
                }
                Reply::BuiltinReasoningThenClose => {
                    "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"thinking\"},\"finish_reason\":null}]}\n\n"
                        .to_string()
                }
                Reply::BuiltinProviderErrorEcho => {
                    let error = json!({"error":{
                        "type": "invalid_request_error",
                        "code": "invalid_request_error",
                        "message": format!("request echo: {ECHO_MARKER}")
                    }});
                    format!("data: {error}\n\n")
                }
                Reply::BuiltinToolThenClose => {
                    let call = json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call-fixture","type":"function","function":{"name":"Bash","arguments":"{\"command\":\"printf a > marker.txt\"}"}}]},"finish_reason":null}]});
                    format!("data: {call}\n\n")
                }
                _ => {
                    let content = json!({"choices":[{"delta":{"content":"fixture answer"},"finish_reason":null}]});
                    let stop = json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}});
                    format!("data: {content}\n\ndata: {stop}\n\ndata: [DONE]\n\n")
                }
            };
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await?;
            socket.shutdown().await
        }
        Reply::Script { steps } => {
            read_request(socket).await?;
            write_script(socket, steps).await
        }
        Reply::RecordScript {
            steps,
            bodies,
            posts,
        } => {
            let request = read_request(socket).await?;
            posts.lock().unwrap().push(std::time::Instant::now());
            bodies.lock().unwrap().push(request);
            write_script(socket, steps).await
        }
        Reply::HeadersThenSilence => {
            read_request(socket).await?;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
                )
                .await?;
            socket.flush().await?;
            tokio::time::sleep(Duration::from_secs(600)).await;
            Ok(())
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

fn provider_config(base: &str, api_format: &str) -> Value {
    json!([{
        "id": "fixture", "name": "Fixture", "base_url": base,
        "api_key": API_KEY, "api_format": api_format, "enabled": true,
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
        std::fs::write(
            temp.path().join("project/config/orchestration.toml"),
            include_str!("../../../config/orchestration.toml")
                .replace("retry_base_backoff_ms = 2000", "retry_base_backoff_ms = 10"),
        )
        .unwrap();
        Self { temp }
    }

    fn data(&self) -> PathBuf {
        self.temp.path().join("data")
    }

    fn workspace(&self) -> PathBuf {
        self.temp.path().join("project")
    }

    fn configure(&self, base: &str, api_format: &str) {
        std::fs::write(
            self.data().join("providers.json"),
            provider_config(base, api_format).to_string(),
        )
        .unwrap();
    }

    /// The built-in OpenCode Go transport, configured through the Desktop-shaped
    /// `config.json` so `settings.builtinId` selects the native adapter.
    fn configure_builtin(&self, base: &str) {
        self.configure_builtin_model(
            base,
            "opencode-go",
            "deepseek-v4.1-flash",
            "openai-completions",
        )
    }

    /// Any built-in provider, including one that speaks the Responses protocol.
    fn configure_builtin_model(&self, base: &str, id: &str, model: &str, api: &str) {
        let config = json!({
            "providers": [{
                "id": id,
                "name": "Fixture",
                "provider_type": "openai",
                "api_key": API_KEY,
                "base_url": base,
                "models": [{
                    "id": model,
                    "name": "Fixture",
                    "enabled": true,
                    "max_tokens": 512,
                    "supports_vision": false,
                    "supports_text": true,
                    "api": api,
                    "supports_tools": true,
                    "supports_streaming": true,
                    "context_window": 128000,
                    "cost_per_1k_input": null,
                    "cost_per_1k_output": null
                }],
                "enabled": true,
                "is_default": true,
                "settings": {"builtinId": id},
                "supports_web_search": false,
                "web_search_strategy": null,
                "web_search_tested_at": null,
                "web_search_test_reason": null
            }],
            "default_provider_id": id
        });
        std::fs::write(self.data().join("config.json"), config.to_string()).unwrap();
    }

    /// A small positive idle deadline and a bounded retry budget, in the cwd
    /// `config/orchestration.toml` the CLI reads before any request.
    fn with_http_policy(&self, idle_secs: u64, retries: u32, backoff_ms: u64) {
        let mut config =
            std::fs::read_to_string(self.workspace().join("config/orchestration.toml")).unwrap();
        config = config.replace(
            "request_timeout_secs = 300",
            &format!("request_timeout_secs = {idle_secs}"),
        );
        config = config.replace("max_retries = 7", &format!("max_retries = {retries}"));
        config = config.replace(
            "retry_base_backoff_ms = 10",
            &format!("retry_base_backoff_ms = {backoff_ms}"),
        );
        deepseek_code_core::data_layout::write_config_file(
            self.workspace().join("config/orchestration.toml"),
            config,
        )
        .unwrap();
    }

    /// Turn on the opt-in post-content replay in the cwd orchestration file.
    fn enable_post_content_recovery(&self) {
        let path = self.workspace().join("config/orchestration.toml");
        let config = std::fs::read_to_string(&path).unwrap().replace(
            "post_content_timeout_recovery = false",
            "post_content_timeout_recovery = true",
        );
        std::fs::write(path, config).unwrap();
    }

    fn builtin_command(&self, session: &str) -> Command {
        self.model_command("opencode-go", "deepseek-v4.1-flash", session)
    }

    fn model_command(&self, provider: &str, model: &str, session: &str) -> Command {
        let mut command = self.stream_command(provider, model);
        command.args(["--session-id", session, "-p", "hello"]);
        command
    }

    /// Resume an existing session for one more print turn; `--resume` and
    /// `--session-id` are mutually exclusive.
    fn resume_command(&self, session: &str) -> Command {
        let mut command = self.stream_command("fixture", "fixture-model");
        command.args(["--resume", session, "-p", "hello"]);
        command
    }

    /// One print turn with stream-json output; the caller selects the session.
    fn stream_command(&self, provider: &str, model: &str) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dcl"));
        command
            .current_dir(self.workspace())
            .args([
                "--data-dir",
                self.data().to_str().unwrap(),
                "--path",
                self.workspace().to_str().unwrap(),
                "--provider",
                provider,
                "--model",
                model,
                "--permission-mode",
                "bypass",
                "--output-format",
                "stream-json",
                "--include-partial-messages",
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
            "DEEPSEEK_CODE_BACKGROUND_JOB",
            "DEEPSEEK_CODE_WORKER_MODEL",
            "DEEPSEEK_CODE_WORKER_AUTO_ROUTE",
        ] {
            command.env_remove(key);
        }
        command
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
            "DEEPSEEK_CODE_BACKGROUND_JOB",
            "DEEPSEEK_CODE_WORKER_MODEL",
            "DEEPSEEK_CODE_WORKER_AUTO_ROUTE",
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
    db.shard_sql(session, |conn| -> anyhow::Result<Value> {
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

/// Every durable turn for a session, oldest first, with the turn ID that the
/// terminal frames use to join them.
fn turn_rows(data: &Path, session: &str) -> Vec<(String, String)> {
    let paths = deepseek_code_core::runtime::RuntimePaths::new(data.to_path_buf()).unwrap();
    let db = paths.open_database().unwrap();
    db.shard_sql(session, |conn| -> anyhow::Result<Vec<(String, String)>> {
        let mut statement = conn.prepare(
            "SELECT id,status FROM execution_turns WHERE conversation_id=?1 ORDER BY started_at, rowid",
        )?;
        let mut rows = statement.query([session])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push((row.get(0)?, row.get(1)?));
        }
        Ok(out)
    })
    .unwrap()
    .unwrap()
}

/// Terminal request records written for each turn, in order.
fn turn_end_records(data: &Path, session: &str) -> Vec<Value> {
    let path = data
        .join("diagnostics/logs/request-attempts")
        .join(format!("{session}.jsonl"));
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event["type"] == "request_turn_end")
        .collect()
}

/// The persisted turn status is what the CLI reports as the turn outcome; it
/// must agree with the diagnostic rather than default to a cancellation.
fn turn_status(data: &Path, session: &str) -> String {
    let paths = deepseek_code_core::runtime::RuntimePaths::new(data.to_path_buf()).unwrap();
    let db = paths.open_database().unwrap();
    db.shard_sql(session, |conn| -> anyhow::Result<String> {
        Ok(conn.query_row(
            "SELECT status FROM execution_turns WHERE conversation_id=?1 ORDER BY started_at DESC LIMIT 1",
            [session],
            |row| row.get(0),
        )?)
    })
    .unwrap()
    .unwrap()
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
    project.configure(&fixture.base, "openai");
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

    let log = std::fs::read_to_string(
        project
            .data()
            .join("diagnostics/logs/request-attempts")
            .join(format!("{SESSION_MARKER}.jsonl")),
    )
    .unwrap();
    let records: Vec<Value> = log
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|event| event["type"] == "request_attempt")
        .collect();
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
    project.configure(&fixture.base, "openai");
    let output = execute(project.command(SESSION_MARKER)).await;
    assert!(!output.status.success());
    assert_eq!(fixture.count(), 1, "authentication failures must not retry");
    let text = combined_output(&output);
    assert!(text.contains("401"), "{text}");
    assert!(text.contains("authentication failed"), "{text}");
    assert!(!text.contains("invalid api key"), "{text}");

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
    let directory = data.join("tasks/background").join(&id);
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
    project.configure(&fixture.base, "openai");
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
    project.configure(&fixture.base, "openai");
    let output = execute(project.command(SESSION_MARKER)).await;
    assert!(
        !output.status.success(),
        "an incomplete stream is a failed turn"
    );
    assert_eq!(
        fixture.count(),
        2,
        "one disconnect, then the answering attempt"
    );
    let text = combined_output(&output);
    for expected in ["attempts=2", "retries=1", "stage=stream", "http=200"] {
        assert!(text.contains(expected), "missing {expected}: {text}");
    }

    let snapshots = persisted_snapshots(&project.data(), SESSION_MARKER);
    let failure = snapshots
        .iter()
        .find_map(|snapshot| snapshot.get("failure").filter(|failure| !failure.is_null()))
        .expect("the incomplete stream is recorded");
    assert_eq!(failure["attempts"], 2, "{failure}");
    assert_eq!(failure["retries"], 1, "{failure}");
    assert_eq!(failure["http"], 200, "{failure}");
    assert_eq!(failure["contentStarted"], true, "{failure}");
}

#[tokio::test]
async fn the_anthropic_protocol_reports_attempts_and_status_after_a_clean_eof() {
    let project = Project::new();
    let fixture = fixture(vec![Reply::AnthropicPartialCleanEnd]).await;
    project.configure(&fixture.base, "anthropic");
    let output = execute(project.command(SESSION_MARKER)).await;
    assert!(
        !output.status.success(),
        "an incomplete stream is a failed turn"
    );
    let text = combined_output(&output);
    assert_eq!(fixture.count(), 1);
    for expected in ["attempts=1", "retries=0", "stage=stream", "http=200"] {
        assert!(text.contains(expected), "missing {expected}: {text}");
    }

    let snapshots = persisted_snapshots(&project.data(), SESSION_MARKER);
    let failure = snapshots
        .iter()
        .find_map(|snapshot| snapshot.get("failure").filter(|failure| !failure.is_null()))
        .expect("the incomplete stream is recorded");
    assert_eq!(failure["attempts"], 1, "{failure}");
    assert_eq!(failure["retries"], 0, "{failure}");
    assert_eq!(failure["http"], 200, "{failure}");
    assert_eq!(failure["stage"], "stream", "{failure}");
    assert_eq!(failure["contentStarted"], true, "{failure}");
}

#[tokio::test]
async fn an_sse_error_event_is_safely_reported_for_both_protocols() {
    for (format, label) in [("openai", "openai"), ("anthropic", "anthropic")] {
        let project = Project::new();
        let fixture = fixture(vec![Reply::SseError {
            anthropic: label == "anthropic",
        }])
        .await;
        project.configure(&fixture.base, format);
        let output = execute(project.command(SESSION_MARKER)).await;
        assert!(!output.status.success(), "a provider error fails the turn");
        assert_eq!(fixture.count(), 1, "{label}: an error event is not retried");
        let text = combined_output(&output);
        assert!(!text.contains(MARKER), "{label}: marker leaked: {text}");
        for expected in ["stage=stream", "http=200", "attempts=1"] {
            assert!(
                text.contains(expected),
                "{label}: missing {expected}: {text}"
            );
        }
        assert_eq!(
            turn_status(&project.data(), SESSION_MARKER),
            "failed",
            "{label}"
        );

        let snapshots = persisted_snapshots(&project.data(), SESSION_MARKER);
        let failure = snapshots
            .iter()
            .find_map(|snapshot| snapshot.get("failure").filter(|failure| !failure.is_null()))
            .unwrap_or_else(|| panic!("{label}: no structured failure: {snapshots:?}"));
        assert_eq!(failure["stage"], "stream", "{label}");
        assert_eq!(failure["http"], 200, "{label}");
        assert_eq!(failure["attempts"], 1, "{label}");
        assert!(!failure.to_string().contains(MARKER), "{label}: {failure}");
    }
}

#[tokio::test]
async fn an_sse_error_records_a_linked_turn_failure() {
    let project = Project::new();
    let fixture = fixture(vec![Reply::SseError { anthropic: false }]).await;
    project.configure(&fixture.base, "openai");
    execute(project.command(SESSION_MARKER)).await;
    // Per-attempt records are appended to a per-session JSONL file, which is
    // what the CLI writes for `/tasks` and the trajectory view.
    let path = project
        .data()
        .join("diagnostics/logs")
        .join("request-attempts")
        .join(format!("{SESSION_MARKER}.jsonl"));
    let record = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("reading {}: {error}", path.display()));
    let parsed: Vec<Value> = record
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let terminal = parsed
        .iter()
        .find(|event| event["type"] == "request_turn_end")
        .expect("a turn-end record");
    assert_eq!(terminal["outcome"], "failed", "{terminal}");
    let failure = terminal
        .get("failure")
        .filter(|failure| !failure.is_null())
        .unwrap_or_else(|| panic!("turn end has no failure: {terminal}"));
    assert_eq!(failure["stage"], "stream", "{failure}");
    assert_eq!(failure["attempts"], 1, "{failure}");
    assert!(!failure.to_string().contains(MARKER), "{failure}");
}

#[tokio::test]
async fn a_partial_stream_end_is_recorded_as_failed_not_cancelled() {
    let project = Project::new();
    let fixture = fixture(vec![Reply::SsePartialCleanEnd]).await;
    project.configure(&fixture.base, "openai");
    let output = execute(project.command(SESSION_MARKER)).await;
    assert!(!output.status.success());
    assert_eq!(fixture.count(), 1, "no replay after visible content");
    assert_eq!(
        turn_status(&project.data(), SESSION_MARKER),
        "failed",
        "a stream that ended early is not a cancellation"
    );
}

#[tokio::test]
async fn a_permanent_quota_error_is_requested_once() {
    let project = Project::new();
    let fixture = fixture(vec![Reply::QuotaError]).await;
    project.configure(&fixture.base, "openai");
    let output = execute(project.command(SESSION_MARKER)).await;
    assert!(!output.status.success());
    let text = combined_output(&output);
    assert_eq!(fixture.count(), 1, "a quota error must not be retried");
    assert!(text.contains("insufficient_quota"), "{text}");
    assert!(text.contains("attempts=1"), "{text}");
    assert!(text.contains("class=permanent"), "{text}");
    assert_eq!(turn_status(&project.data(), SESSION_MARKER), "failed");
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
    project.configure(&fixture.base, "openai");
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
        .shard_sql(SESSION_MARKER, |conn| -> anyhow::Result<String> {
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

fn stream_events(output: &Output) -> Vec<Value> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.starts_with('{'))
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn request_attempts(data: &Path, session: &str) -> Vec<Value> {
    let path = data
        .join("diagnostics/logs/request-attempts")
        .join(format!("{session}.jsonl"));
    std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("reading attempts: {error}"))
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event: &Value| event["type"] == "request_attempt")
        .collect()
}

/// The same records, but an absent log is an empty history: used while polling
/// for an event the running process may not have written yet.
fn observed_attempts(data: &Path, session: &str) -> Vec<Value> {
    let path = data
        .join("diagnostics/logs/request-attempts")
        .join(format!("{session}.jsonl"));
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event: &Value| event["type"] == "request_attempt")
        .collect()
}

#[tokio::test]
async fn builtin_chat_retry_reports_one_logical_response_and_known_attempts() {
    let project = Project::new();
    let fixture = fixture(vec![Reply::BuiltinEmptyThenClose, Reply::BuiltinComplete]).await;
    project.configure_builtin(&fixture.base);
    let output = execute(project.builtin_command(SESSION_MARKER)).await;
    let text = combined_output(&output);
    assert!(output.status.success(), "{text}");
    assert_eq!(
        fixture.count(),
        2,
        "one drop, then the answering attempt: {text}"
    );

    let events = stream_events(&output);
    assert_eq!(
        events
            .iter()
            .filter(|event| event["type"] == "message_start")
            .count(),
        1,
        "one logical response: {text}"
    );
    let results: Vec<_> = events
        .iter()
        .filter(|event| event["type"] == "result")
        .collect();
    assert_eq!(results.len(), 1, "{text}");
    assert_eq!(results[0]["is_error"], false, "{}", results[0]);
    assert_eq!(results[0]["num_turns"], 1, "{}", results[0]);
    let deltas: String = events
        .iter()
        .filter(|event| event["type"] == "text")
        .filter_map(|event| event["delta"].as_str())
        .collect();
    assert_eq!(deltas.matches("fixture answer").count(), 1, "{deltas}");

    let attempts = request_attempts(&project.data(), SESSION_MARKER);
    let received: Vec<_> = attempts
        .iter()
        .filter(|event| event["outcome"] == "response_received")
        .collect();
    assert_eq!(received.len(), 2, "{attempts:?}");
    let waits: Vec<_> = attempts
        .iter()
        .filter(|event| event["outcome"] == "retry_wait")
        .collect();
    assert_eq!(waits.len(), 1, "{attempts:?}");
    assert_eq!(waits[0]["waitMs"], 10, "{}", waits[0]);
    let request_id = received[0]["requestId"].as_str().unwrap().to_owned();
    assert_eq!(received[0]["attempts"], 1);
    assert_eq!(received[1]["attempts"], 2);
    assert!(
        received
            .iter()
            .all(|event| event["requestId"] == request_id.as_str()),
        "retries share one request ID: {attempts:?}"
    );

    let snapshots = persisted_snapshots(&project.data(), SESSION_MARKER);
    let snapshot = snapshots
        .iter()
        .find(|snapshot| snapshot["id"] == request_id.as_str())
        .unwrap_or_else(|| panic!("no snapshot with stable id {request_id}: {snapshots:?}"));
    assert_eq!(snapshot["status"], "completed");
}

async fn builtin_failure(reply: Reply) -> (Output, usize, Value, bool) {
    let project = Project::new();
    let fixture = fixture(vec![reply]).await;
    project.configure_builtin(&fixture.base);
    let output = execute(project.builtin_command(SESSION_MARKER)).await;
    let requests = fixture.count();
    let marker = project.workspace().join("marker.txt").exists();
    let snapshots = persisted_snapshots(&project.data(), SESSION_MARKER);
    let failure = snapshots
        .iter()
        .find_map(|snapshot| snapshot.get("failure").filter(|failure| !failure.is_null()))
        .cloned()
        .unwrap_or_else(|| panic!("no structured failure: {snapshots:?}"));
    (output, requests, failure, marker)
}

#[tokio::test]
async fn builtin_chat_reasoning_and_tool_only_failures_report_true_content() {
    for (name, reply) in [
        ("reasoning", Reply::BuiltinReasoningThenClose),
        ("tool", Reply::BuiltinToolThenClose),
    ] {
        let (output, requests, failure, marker) = builtin_failure(reply).await;
        let text = combined_output(&output);
        assert!(
            !output.status.success(),
            "{name}: a pre-completion EOF fails"
        );
        assert_eq!(requests, 1, "{name}: content must not be replayed: {text}");
        for expected in [
            "content_started=true",
            "http=200",
            "attempts=1",
            "retries=0",
        ] {
            assert!(
                text.contains(expected),
                "{name}: missing {expected}: {text}"
            );
        }
        assert_eq!(failure["contentStarted"], true, "{name}: {failure}");
        assert_eq!(failure["http"], 200, "{name}: {failure}");
        assert_eq!(failure["attempts"], 1, "{name}: {failure}");
        assert!(!marker, "{name}: no tool may be dispatched");
        let events = stream_events(&output);
        assert!(
            events.iter().all(|event| event["type"] != "tool_start"),
            "{name}: no tool may be dispatched: {text}"
        );
    }
}

#[tokio::test]
async fn builtin_chat_provider_error_echo_is_absent_from_attempt_diagnostics() {
    let project = Project::new();
    let fixture = fixture(vec![
        Reply::BuiltinProviderErrorEcho,
        Reply::BuiltinComplete,
    ])
    .await;
    project.configure_builtin(&fixture.base);
    let output = execute(project.builtin_command(SESSION_MARKER)).await;
    let text = combined_output(&output);
    assert_eq!(fixture.count(), 1, "a provider error was replayed: {text}");
    assert_eq!(output.status.code(), Some(1), "{text}");
    assert!(
        !text.contains(ECHO_MARKER),
        "the echoed provider text leaked to the terminal: {text}"
    );

    // The event is emitted on stdout and persisted to the attempt log; both
    // must carry the safe structured failure, not the provider's echo.
    let events = stream_events(&output);
    let emitted = events
        .iter()
        .find(|event| event["type"] == "request_attempt" && event["outcome"] == "stream_failed")
        .unwrap_or_else(|| panic!("no stream_failed event was emitted: {text}"));
    let attempts = request_attempts(&project.data(), SESSION_MARKER);
    let persisted = attempts
        .iter()
        .find(|event| event["outcome"] == "stream_failed")
        .unwrap_or_else(|| panic!("no persisted stream_failed record: {attempts:?}"));
    for (name, event) in [("emitted", emitted), ("persisted", persisted)] {
        assert!(
            !event.to_string().contains(ECHO_MARKER),
            "{name}: the echo reached the attempt diagnostic: {event}"
        );
        assert_eq!(event["failure"]["stage"], "stream", "{name}: {event}");
        assert_eq!(event["failure"]["http"], 200, "{name}: {event}");
        assert_eq!(event["failure"]["attempts"], 1, "{name}: {event}");
        assert!(
            event["failure"]["cause"]
                .as_str()
                .is_some_and(|cause| !cause.is_empty()),
            "{name}: no safe cause was reported: {event}"
        );
    }
    assert!(
        attempts
            .iter()
            .all(|event| event["outcome"] != "retry_wait"),
        "a provider error scheduled a reconnect: {attempts:?}"
    );

    let snapshots = persisted_snapshots(&project.data(), SESSION_MARKER);
    let failure = snapshots
        .iter()
        .find_map(|snapshot| snapshot.get("failure").filter(|failure| !failure.is_null()))
        .cloned()
        .unwrap_or_else(|| panic!("no structured failure: {snapshots:?}"));
    assert!(
        !failure.to_string().contains(ECHO_MARKER),
        "the echo reached the SQLite snapshot: {failure}"
    );
    assert_eq!(failure["stage"], "stream", "{failure}");
    assert_eq!(failure["http"], 200, "{failure}");
    assert_eq!(failure["attempts"], 1, "{failure}");
    assert_eq!(failure["retries"], 0, "{failure}");
}

// ---- Streaming idle-timeout fixtures (round B) ----

const CHAT_TICK: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"tick \"},\"finish_reason\":null}]}\n\n";
const CHAT_STOP: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":9}}\n\ndata: [DONE]\n\n";
const CHAT_PARTIAL: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial answer\"},\"finish_reason\":null}]}\n\n";
const CHAT_RAW_UNFINISHED: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"truncated";
const CHAT_RAW_DONE_ONLY: &str = "data: [DONE]\n\n";
const CHAT_RAW_OPAQUE: &str = "data: {\"choices\":[{\"delta\":{\"content\":[{\"type\":\"fixture-extension\",\"metadata\":{\"state\":\"present\"}}]},\"finish_reason\":null}]}\n\n";

const ANTHROPIC_START: &str = "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"model\":\"fixture\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n";
const ANTHROPIC_BLOCK: &str = "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n";
const ANTHROPIC_TICK: &str = "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"tick \"}}\n\n";
const ANTHROPIC_PARTIAL: &str = "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial answer\"}}\n\n";
const ANTHROPIC_STOP: &str =
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\ndata: {\"type\":\"message_stop\"}\n\n";

const RESPONSES_CREATED: &str =
    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\",\"model\":\"fixture\"}}\n\n";
const RESPONSES_TICK: &str =
    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"tick \"}\n\n";
const RESPONSES_PARTIAL: &str =
    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial answer\"}\n\n";
const RESPONSES_DONE: &str = "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"model\":\"fixture\",\"status\":\"completed\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":9,\"total_tokens\":10}}}\n\n";

#[derive(Clone, Copy, PartialEq)]
enum Transport {
    BuiltinChat,
    OpenAI,
    Anthropic,
    BuiltinResponses,
}

impl Transport {
    fn label(self) -> &'static str {
        match self {
            Self::BuiltinChat => "builtin-chat",
            Self::OpenAI => "direct-openai",
            Self::Anthropic => "direct-anthropic",
            Self::BuiltinResponses => "builtin-responses",
        }
    }

    fn configure(self, project: &Project, base: &str) {
        match self {
            Self::BuiltinChat => project.configure_builtin(base),
            Self::BuiltinResponses => {
                project.configure_builtin_model(base, "openai", "fixture-model", "openai-responses")
            }
            Self::OpenAI => project.configure(base, "openai"),
            Self::Anthropic => project.configure(base, "anthropic"),
        }
    }

    fn command(self, project: &Project, session: &str) -> Command {
        match self {
            Self::BuiltinChat => {
                project.model_command("opencode-go", "deepseek-v4.1-flash", session)
            }
            Self::BuiltinResponses => project.model_command("openai", "fixture-model", session),
            _ => project.model_command("fixture", "fixture-model", session),
        }
    }

    /// A complete answer whose bytes keep arriving for longer than one idle
    /// window, so only an idle deadline (not a total one) lets it finish.
    fn healthy(self, ticks: usize, gap_ms: u64) -> Vec<Step> {
        let mut steps = Vec::new();
        match self {
            Self::BuiltinChat => {
                for _ in 0..ticks {
                    steps.extend([Step::Write(CHAT_TICK), Step::Pause(gap_ms)]);
                }
                steps.push(Step::Write(CHAT_STOP));
            }
            Self::OpenAI => {
                for _ in 0..ticks {
                    steps.extend([Step::Write(CHAT_TICK), Step::Pause(gap_ms)]);
                }
                steps.push(Step::Write(CHAT_STOP));
            }
            Self::Anthropic => {
                steps.push(Step::Write(ANTHROPIC_START));
                steps.push(Step::Write(ANTHROPIC_BLOCK));
                for _ in 0..ticks {
                    steps.extend([Step::Write(ANTHROPIC_TICK), Step::Pause(gap_ms)]);
                }
                steps.push(Step::Write(ANTHROPIC_STOP));
            }
            Self::BuiltinResponses => {
                steps.push(Step::Write(RESPONSES_CREATED));
                for _ in 0..ticks {
                    steps.extend([Step::Write(RESPONSES_TICK), Step::Pause(gap_ms)]);
                }
                steps.push(Step::Write(RESPONSES_DONE));
            }
        }
        steps
    }

    /// Content arrives and then the body goes silent forever.
    fn post_content_stall(self) -> Vec<Step> {
        let frame = match self {
            Self::Anthropic => ANTHROPIC_PARTIAL,
            Self::BuiltinResponses => RESPONSES_PARTIAL,
            _ => CHAT_PARTIAL,
        };
        let mut steps = Vec::new();
        if self == Self::Anthropic {
            steps.push(Step::Write(ANTHROPIC_START));
            steps.push(Step::Write(ANTHROPIC_BLOCK));
        }
        if self == Self::BuiltinResponses {
            steps.push(Step::Write(RESPONSES_CREATED));
        }
        steps.extend([Step::Write(frame), Step::Hold]);
        steps
    }
}

fn text_deltas(output: &Output) -> String {
    stream_events(output)
        .iter()
        .filter(|event| event["type"] == "text")
        .filter_map(|event| event["delta"].as_str())
        .collect()
}

fn persisted_failure(data: &Path, session: &str) -> Value {
    persisted_snapshots(data, session)
        .iter()
        .find_map(|snapshot| snapshot.get("failure").filter(|failure| !failure.is_null()))
        .cloned()
        .unwrap_or_else(|| panic!("no structured failure recorded"))
}

/// Keep the measured timing and request count in the test log for the record.
fn note(case: &str, started: std::time::Instant, posts: usize, output: &Output) {
    eprintln!(
        "{case}: elapsed={:?} posts={posts} exit={:?}",
        started.elapsed(),
        output.status.code()
    );
}

#[tokio::test]
async fn stream_timeout_uses_idle_configuration_for_all_model_transports() {
    const IDLE: u64 = 1;
    let transports = [
        Transport::BuiltinChat,
        Transport::OpenAI,
        Transport::Anthropic,
        Transport::BuiltinResponses,
    ];

    // 1. A healthy stream that lasts longer than one idle window still finishes.
    for transport in transports {
        let name = transport.label();
        let project = Project::new();
        project.with_http_policy(IDLE, 2, 20);
        let fixture = fixture(vec![Reply::Script {
            steps: transport.healthy(8, 400),
        }])
        .await;
        transport.configure(&project, &fixture.base);
        let started = std::time::Instant::now();
        let output = execute(transport.command(&project, SESSION_MARKER)).await;
        let elapsed = started.elapsed();
        let text = combined_output(&output);
        assert!(
            output.status.success(),
            "{name}: a healthy stream must finish: {text}"
        );
        assert_eq!(fixture.count(), 1, "{name}: {text}");
        assert!(
            elapsed > Duration::from_secs(IDLE),
            "{name}: the stream must outlive one idle window: {elapsed:?}"
        );
        assert_eq!(
            text_deltas(&output).matches("tick ").count(),
            8,
            "{name}: the complete output must be kept: {text}"
        );
        note(
            &format!("{name}/healthy"),
            started,
            fixture.count(),
            &output,
        );
    }

    // 2. Content, then silence: one POST, a stream error, partial text retained.
    for transport in transports {
        let name = transport.label();
        let project = Project::new();
        project.with_http_policy(IDLE, 2, 20);
        let fixture = fixture(vec![Reply::Script {
            steps: transport.post_content_stall(),
        }])
        .await;
        transport.configure(&project, &fixture.base);
        let started = std::time::Instant::now();
        let output = execute(transport.command(&project, SESSION_MARKER)).await;
        let text = combined_output(&output);
        assert_eq!(output.status.code(), Some(1), "{name}: {text}");
        assert_eq!(
            fixture.count(),
            1,
            "{name}: content must not be replayed: {text}"
        );
        assert!(text.contains("stage=stream"), "{name}: {text}");
        // Only the Chat/direct paths share one budget with the body, so only
        // they can report the observed status.
        if transport != Transport::BuiltinResponses {
            assert!(text.contains("http=200"), "{name}: {text}");
        }
        assert_eq!(
            text_deltas(&output).matches("partial answer").count(),
            1,
            "{name}: partial text must be retained once: {text}"
        );
        let failure = persisted_failure(&project.data(), SESSION_MARKER);
        assert_eq!(failure["stage"], "stream", "{name}: {failure}");
        if transport != Transport::BuiltinResponses {
            assert_eq!(failure["http"], 200, "{name}: {failure}");
            assert_eq!(failure["attempts"], 1, "{name}: {failure}");
        }
        note(
            &format!("{name}/post-content"),
            started,
            fixture.count(),
            &output,
        );
    }

    // 3. Flushed headers with no body bytes: an idle failure, not a send one.
    for transport in transports {
        let name = transport.label();
        let project = Project::new();
        project.with_http_policy(IDLE, 2, 20);
        let fixture = fixture(vec![Reply::HeadersThenSilence]).await;
        transport.configure(&project, &fixture.base);
        let started = std::time::Instant::now();
        let output = execute(transport.command(&project, SESSION_MARKER)).await;
        let text = combined_output(&output);
        assert_eq!(output.status.code(), Some(1), "{name}: {text}");
        // Chat and the direct clients retry a body that produced nothing; the
        // other built-in protocols keep A's send-only policy.
        let expected = if transport == Transport::BuiltinResponses {
            1
        } else {
            3
        };
        assert_eq!(
            fixture.count(),
            expected,
            "{name}: unexpected number of POSTs: {text}"
        );
        let failure = persisted_failure(&project.data(), SESSION_MARKER);
        assert_eq!(failure["stage"], "stream", "{name}: {failure}");
        if transport != Transport::BuiltinResponses {
            assert_eq!(failure["http"], 200, "{name}: {failure}");
        }
        assert!(
            text_deltas(&output).is_empty(),
            "{name}: no text may be produced: {text}"
        );
        note(
            &format!("{name}/headers-only"),
            started,
            fixture.count(),
            &output,
        );
    }

    // 4. With a retry budget, Chat and direct Anthropic recover from an empty
    // pre-content body; the second attempt answers.
    for transport in [Transport::BuiltinChat, Transport::Anthropic] {
        let name = transport.label();
        let project = Project::new();
        project.with_http_policy(IDLE, 2, 20);
        let fixture = fixture(vec![
            Reply::HeadersThenSilence,
            Reply::Script {
                steps: transport.healthy(2, 50),
            },
        ])
        .await;
        transport.configure(&project, &fixture.base);
        let output = execute(transport.command(&project, SESSION_MARKER)).await;
        let text = combined_output(&output);
        assert!(
            output.status.success(),
            "{name}: recovery must succeed: {text}"
        );
        assert_eq!(
            fixture.count(),
            2,
            "{name}: one retry, then success: {text}"
        );
        assert_eq!(
            text_deltas(&output).matches("tick ").count(),
            2,
            "{name}: the recovering answer must be complete: {text}"
        );
    }

    // 5. Built-in Responses keeps A's send-only policy: a silent body is not
    // retried even though a successful reply is waiting.
    {
        let project = Project::new();
        project.with_http_policy(IDLE, 2, 20);
        let fixture = fixture(vec![
            Reply::HeadersThenSilence,
            Reply::Script {
                steps: Transport::BuiltinResponses.healthy(2, 50),
            },
        ])
        .await;
        Transport::BuiltinResponses.configure(&project, &fixture.base);
        let output = execute(Transport::BuiltinResponses.command(&project, SESSION_MARKER)).await;
        let text = combined_output(&output);
        assert_eq!(output.status.code(), Some(1), "{text}");
        assert_eq!(fixture.count(), 1, "Responses must not replay: {text}");
    }

    // 6. Cancelling during the observed backoff sends nothing else. The job
    // control directory is the same fixture `/tasks cancel` writes, so the case
    // is portable and no external signal utility is required.
    {
        const BACKOFF_MS: u64 = 2000;
        let project = Project::new();
        project.with_http_policy(IDLE, 3, BACKOFF_MS);
        let fixture = fixture(vec![Reply::HeadersThenSilence]).await;
        Transport::BuiltinChat.configure(&project, &fixture.base);
        let directory = worker_directory(&project.data(), SESSION_MARKER);
        let mut command = Transport::BuiltinChat.command(&project, SESSION_MARKER);
        command.env(
            deepseek_code_core::services::background_sessions::CONTROL_ENV,
            &directory,
        );
        let mut child = command.spawn().unwrap();
        fixture.wait_for_first_request().await;
        // The scheduled wait must have begun, not merely elapsed, before the
        // cancel is written.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while !observed_attempts(&project.data(), SESSION_MARKER)
            .iter()
            .any(|event| event["outcome"] == "retry_wait")
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the transport never entered its retry wait"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        std::fs::write(directory.join("cancel"), "").unwrap();
        let status = tokio::time::timeout(Duration::from_secs(20), child.wait())
            .await
            .expect("the CLI ignored the cancellation")
            .unwrap();
        assert!(!status.success(), "a cancelled turn must not exit zero");
        // Well past the scheduled retry instant: nothing may be sent again.
        tokio::time::sleep(Duration::from_millis(BACKOFF_MS + 800)).await;
        assert_eq!(
            fixture.count(),
            1,
            "a cancelled retry wait must not send another POST"
        );
        let wait = observed_attempts(&project.data(), SESSION_MARKER)
            .iter()
            .find(|event| event["outcome"] == "retry_wait")
            .and_then(|event| event["waitMs"].as_u64())
            .unwrap_or_default();
        eprintln!("builtin-chat/backoff-cancel: retry_wait observed {wait}ms");
    }

    // 7. A's raw replay barriers still hold under the idle deadline.
    for raw in [CHAT_RAW_UNFINISHED, CHAT_RAW_DONE_ONLY, CHAT_RAW_OPAQUE] {
        let project = Project::new();
        project.with_http_policy(IDLE, 2, 20);
        let fixture = fixture(vec![Reply::Script {
            steps: vec![Step::Write(raw), Step::Hold],
        }])
        .await;
        Transport::BuiltinChat.configure(&project, &fixture.base);
        let output = execute(Transport::BuiltinChat.command(&project, SESSION_MARKER)).await;
        let text = combined_output(&output);
        assert_eq!(output.status.code(), Some(1), "{text}");
        assert_eq!(
            fixture.count(),
            1,
            "a raw barrier was replayed: {raw:?} {text}"
        );
        let attempts = request_attempts(&project.data(), SESSION_MARKER);
        assert!(
            attempts
                .iter()
                .all(|event| event["outcome"] != "retry_wait"),
            "a raw barrier scheduled a reconnect: {attempts:?}"
        );
        let failure = persisted_failure(&project.data(), SESSION_MARKER);
        assert_eq!(failure["stage"], "stream", "{failure}");
        assert_eq!(failure["http"], 200, "{failure}");
        assert_eq!(failure["attempts"], 1, "{failure}");
        assert_eq!(failure["retries"], 0, "{failure}");
    }
}

/// One-shot `--print` fails once on a post-text idle timeout, and the same
/// session can be resumed afterwards without disturbing the failed turn.
#[tokio::test]
async fn single_print_model_stream_timeout_fails_once() {
    const IDLE: u64 = 1;
    let project = Project::new();
    project.with_http_policy(IDLE, 2, 20);
    let failed_fixture = fixture(vec![Reply::Script {
        steps: vec![Step::Write(CHAT_PARTIAL), Step::Hold],
    }])
    .await;
    project.configure(&failed_fixture.base, "openai");
    let started = std::time::Instant::now();
    let output = execute(project.model_command("fixture", "fixture-model", SESSION_MARKER)).await;
    let text = combined_output(&output);
    assert_eq!(output.status.code(), Some(1), "{text}");
    assert_eq!(
        failed_fixture.count(),
        1,
        "content must not be replayed: {text}"
    );
    note(
        "single-print/first",
        started,
        failed_fixture.count(),
        &output,
    );

    let events = stream_events(&output);
    let results: Vec<&Value> = events
        .iter()
        .filter(|event| event["type"] == "result")
        .collect();
    assert_eq!(results.len(), 1, "one result: {text}");
    assert_eq!(results[0]["is_error"], true, "{}", results[0]);
    assert_eq!(results[0]["stop_reason"], "stream_error", "{}", results[0]);
    assert_eq!(results[0]["session_id"], SESSION_MARKER);
    assert_eq!(
        text_deltas(&output).matches("partial answer").count(),
        1,
        "partial text must be retained once: {text}"
    );
    let turn_id = results[0]["turn"]["turnId"]
        .as_str()
        .expect("the result carries its turn")
        .to_owned();
    assert_eq!(
        turn_rows(&project.data(), SESSION_MARKER),
        vec![(turn_id.clone(), "failed".to_owned())]
    );
    let ends = turn_end_records(&project.data(), SESSION_MARKER);
    assert_eq!(ends.len(), 1, "one terminal record per turn: {ends:?}");
    assert_eq!(ends[0]["turnId"], turn_id.as_str());
    assert_eq!(ends[0]["outcome"], "failed", "{}", ends[0]);

    // A separate process resumes the same session against a succeeding reply.
    let resumed = fixture(vec![Reply::Script {
        steps: vec![
            Step::Write(
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"resumed answer\"},\"finish_reason\":null}]}\n\n",
            ),
            Step::Write(CHAT_STOP),
        ],
    }])
    .await;
    project.configure(&resumed.base, "openai");
    let started = std::time::Instant::now();
    let output = execute(project.resume_command(SESSION_MARKER)).await;
    let text = combined_output(&output);
    assert!(output.status.success(), "resume must succeed: {text}");
    assert_eq!(resumed.count(), 1, "one new POST: {text}");
    note("single-print/resume", started, resumed.count(), &output);
    let events = stream_events(&output);
    let results: Vec<&Value> = events
        .iter()
        .filter(|event| event["type"] == "result")
        .collect();
    assert_eq!(results.len(), 1, "one result: {text}");
    assert_eq!(results[0]["is_error"], false, "{}", results[0]);
    assert_eq!(results[0]["session_id"], SESSION_MARKER);
    assert_eq!(
        text_deltas(&output).matches("resumed answer").count(),
        1,
        "{text}"
    );
    // The failed row is untouched; the resumed turn is a second, completed one.
    let rows = turn_rows(&project.data(), SESSION_MARKER);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0], (turn_id, "failed".to_owned()));
    assert_eq!(rows[1].1, "completed");
    assert_ne!(rows[1].0, rows[0].0);
    let ends = turn_end_records(&project.data(), SESSION_MARKER);
    assert_eq!(ends.len(), 2, "{ends:?}");
    assert_eq!(ends[1]["outcome"], "completed", "{}", ends[1]);
}

/// The JSON body of one captured HTTP request.
fn request_body(raw: &[u8]) -> Value {
    let end = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("a complete request head")
        + 4;
    serde_json::from_slice(&raw[end..]).expect("a JSON request body")
}

/// Every JSON line the CLI wrote to stdout.
fn stream_frames(output: &Output) -> Vec<Value> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .collect()
}

/// The recovery notices written to the attempt log.
fn post_content_notices(data: &Path, session: &str) -> Vec<Value> {
    observed_attempts(data, session)
        .into_iter()
        .filter(|event| event["outcome"] == "post_content_retry")
        .collect()
}

const RECOVERY_TOOL_ROUND: &str = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-fixture\",\"type\":\"function\",\"function\":{\"name\":\"Bash\",\"arguments\":\"{\\\"command\\\":\\\"printf one >> marker.txt\\\"}\"}}]},\"finish_reason\":null}]}\n\n";
const RECOVERY_TOOL_STOP: &str = "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2}}\n\n";
const ABANDONED_THINKING: &str = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"ABANDONED_THINK\"},\"finish_reason\":null}]}\n\n";
const ABANDONED_TEXT: &str =
    "data: {\"choices\":[{\"delta\":{\"content\":\"ABANDONED_TEXT\"},\"finish_reason\":null}]}\n\n";
const ABANDONED_TOOL: &str = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-orphan\",\"type\":\"function\",\"function\":{\"name\":\"Write\",\"arguments\":\"{\\\"file_path\\\":\\\"orphan.txt\\\"\"}}]},\"finish_reason\":null}]}\n\n";
const ABANDONED_USAGE: &str =
    "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":55,\"completion_tokens\":3}}\n\n";
const RECOVERED_TEXT: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"recovered answer\"},\"finish_reason\":null}]}\n\n";
const RECOVERED_STOP: &str = "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":111,\"completion_tokens\":7}}\n\n";

#[tokio::test]
async fn post_content_timeout_replays_only_the_uncommitted_model_step() {
    for transport in [Transport::BuiltinChat, Transport::OpenAI] {
        let label = transport.label();
        let project = Project::new();
        project.enable_post_content_recovery();
        // A budget of three keeps the first leg inside the application limit,
        // so an unknown or already spent count cannot veto this case. The
        // backoff is long enough to order the notice against the replacement.
        project.with_http_policy(1, 2, 300);
        let bodies: Arc<std::sync::Mutex<Vec<Vec<u8>>>> = Default::default();
        let posts: Arc<std::sync::Mutex<Vec<std::time::Instant>>> = Default::default();
        let hold = Arc::new(tokio::sync::Notify::new());
        let fixture = fixture(vec![
            Reply::Script {
                steps: vec![
                    Step::Write(RECOVERY_TOOL_ROUND),
                    Step::Write(RECOVERY_TOOL_STOP),
                    Step::Write("data: [DONE]\n\n"),
                ],
            },
            Reply::RecordScript {
                steps: vec![
                    Step::Write(ABANDONED_THINKING),
                    Step::Write(ABANDONED_TEXT),
                    Step::Write(ABANDONED_TOOL),
                    Step::Write(ABANDONED_USAGE),
                    Step::Signal(hold.clone()),
                    Step::Hold,
                ],
                bodies: bodies.clone(),
                posts: posts.clone(),
            },
            Reply::RecordScript {
                steps: vec![
                    Step::Write(RECOVERED_TEXT),
                    Step::Write(RECOVERED_STOP),
                    Step::Write("data: [DONE]\n\n"),
                ],
                bodies: bodies.clone(),
                posts: posts.clone(),
            },
        ])
        .await;
        transport.configure(&project, &fixture.base);
        let (status, stdout, frames, _windowed, stderr) = run_streaming_frames(
            transport.command(&project, SESSION_MARKER),
            hold,
            Duration::from_millis(200),
        )
        .await;
        let text = format!("{stdout}{stderr}");

        assert!(status.success(), "{label}: {text}");
        assert_eq!(fixture.count(), 3, "{label}: one POST per leg: {text}");
        // The committed step ran its tool exactly once; the unfinished Write never ran.
        let marker =
            std::fs::read_to_string(project.workspace().join("marker.txt")).unwrap_or_default();
        assert_eq!(marker, "one", "{label}: the tool ran more than once");
        assert!(
            !project.workspace().join("orphan.txt").exists(),
            "{label}: an unfinished tool call was dispatched"
        );
        // Only the replacement's output is visible.
        let results: Vec<&Value> = frames
            .iter()
            .map(|(frame, _)| frame)
            .filter(|frame| frame["type"] == "result")
            .collect();
        assert_eq!(results.len(), 1, "{label}: {text}");
        let result = results[0];
        assert_eq!(result["is_error"], false, "{label}: {result}");
        let answer = result["result"].as_str().unwrap_or_default();
        assert_eq!(
            answer.matches("recovered answer").count(),
            1,
            "{label}: {answer}"
        );
        assert!(!answer.contains("ABANDONED"), "{label}: {answer}");
        // Two logical responses: the committed step and the replacement. A
        // duplicate start on the replacement would make this three.
        assert_eq!(
            frames
                .iter()
                .filter(|(frame, _)| frame["type"] == "message_start")
                .count(),
            2,
            "{label}: {text}"
        );

        // Exactly one notice, before the replacement, naming the discarded step.
        let notices = post_content_notices(&project.data(), SESSION_MARKER);
        assert_eq!(notices.len(), 1, "{label}: {notices:?}");
        assert_eq!(notices[0]["attempts"], 1, "{label}: {}", notices[0]);
        assert!(
            notices[0]["failure"]["stage"] == "stream",
            "{label}: {}",
            notices[0]
        );
        let streamed: Vec<&(Value, std::time::Instant)> = frames
            .iter()
            .filter(|(frame, _)| frame["outcome"] == "post_content_retry")
            .collect();
        assert_eq!(
            streamed.len(),
            1,
            "{label}: the notice was not streamed once"
        );
        let replacement_post = {
            let recorded = posts.lock().unwrap();
            // Only the two replayed legs record their arrival; the first POST
            // uses a plain script. The last entry is the replacement.
            assert_eq!(recorded.len(), 2, "{label}: POST times: {recorded:?}");
            recorded[1]
        };
        assert!(
            streamed[0].1 < replacement_post,
            "{label}: the notice was not written before the replacement POST"
        );
        assert!(
            !observed_attempts(&project.data(), SESSION_MARKER)
                .iter()
                .any(|event| event["outcome"] == "retry_wait"),
            "{label}: the first leg entered a send retry"
        );

        // Both legs carried the same committed context: the prior tool call and
        // its result, plus this turn's user input.
        let recorded = bodies.lock().unwrap();
        assert_eq!(recorded.len(), 2, "{label}: both legs were captured");
        let first: Value = request_body(&recorded[0]);
        let second: Value = request_body(&recorded[1]);
        assert_eq!(first["messages"], second["messages"], "{label}");
        let context = first["messages"].as_array().unwrap();
        assert!(
            context.iter().any(|message| message["role"] == "tool"),
            "{label}: the prior tool result is missing"
        );
        assert!(
            context
                .iter()
                .any(|message| message["role"] == "assistant" && !message["tool_calls"].is_null()),
            "{label}: the prior tool call is missing"
        );

        // Two linked snapshots for the replayed step, and no failure on the turn.
        let snapshots = persisted_snapshots(&project.data(), SESSION_MARKER);
        let step_two: Vec<&Value> = snapshots
            .iter()
            .filter(|snapshot| snapshot["number"] == 2)
            .collect();
        assert_eq!(step_two.len(), 2, "{label}: {step_two:?}");
        let abandoned = step_two
            .iter()
            .find(|snapshot| snapshot["abandonedForRecovery"] == true)
            .unwrap_or_else(|| panic!("{label}: no abandoned snapshot"));
        let replacement = step_two
            .iter()
            .find(|snapshot| snapshot["abandonedForRecovery"] != true)
            .unwrap();
        assert_eq!(
            replacement["replacesRequestId"], abandoned["id"],
            "{label}: the snapshots are not linked"
        );
        assert_ne!(replacement["id"], abandoned["id"], "{label}");
        assert_eq!(
            abandoned["usage"]["prompt_tokens"], 55,
            "{label}: {}",
            abandoned
        );
        assert_eq!(
            replacement["usage"]["prompt_tokens"], 111,
            "{label}: {}",
            replacement
        );
        assert_eq!(result["usage"]["prompt_tokens"], 176, "{label}: {result}");
        assert_eq!(
            result["usage"]["completion_tokens"], 12,
            "{label}: {result}"
        );
        // The abandoned leg is diagnostic only: the step ends without its blocks.
        assert!(
            replacement["responseBlocks"]
                .to_string()
                .contains("recovered answer"),
            "{label}: {replacement}"
        );
        assert_eq!(
            turn_status(&project.data(), SESSION_MARKER),
            "completed",
            "{label}"
        );
        let ends = turn_end_records(&project.data(), SESSION_MARKER);
        assert_eq!(ends.len(), 1, "{label}: {ends:?}");
        assert_eq!(ends[0]["outcome"], "completed", "{label}: {}", ends[0]);
        assert!(
            ends[0]["failure"].is_null(),
            "{label}: the completed turn owns a failure: {}",
            ends[0]
        );
        // The notice names the stalled leg the abandoned snapshot belongs to.
        assert_eq!(
            notices[0]["requestId"], abandoned["id"],
            "{label}: the notice does not name the stalled request"
        );
        // Nothing abandoned reaches the host, stderr or the stored transcript.
        for marker in ["ABANDONED_THINK", "ABANDONED_TEXT", "call-orphan"] {
            assert!(!text.contains(marker), "{label}: {marker} leaked: {text}");
        }
        let (stored, thinking) = stored_assistant(&project.data(), SESSION_MARKER);
        assert!(!stored.contains("ABANDONED"), "{label}: {stored}");
        assert!(!thinking.contains("ABANDONED"), "{label}: {thinking}");
        assert_eq!(
            stored.matches("recovered answer").count(),
            1,
            "{label}: {stored}"
        );
        // The replacement was sent the committed context only.
        let replacement_context = second["messages"].to_string();
        assert!(
            !replacement_context.contains("ABANDONED") && !replacement_context.contains("orphan"),
            "{label}: the abandoned attempt reached the replacement: {replacement_context}"
        );
        let calls = tool_rows(&project.data(), SESSION_MARKER);
        assert_eq!(calls.len(), 1, "{label}: {calls:?}");
        assert_eq!(calls[0].0, "Bash", "{label}: {calls:?}");
        assert!(
            !calls.iter().any(|(_, input)| input.contains("orphan")),
            "{label}: the abandoned tool call was stored: {calls:?}"
        );
    }
}

const LIVE_TEXT: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"VISIBLE_PARTIAL\"},\"finish_reason\":null}]}\n\n";
const LIVE_THINKING: &str = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"THINK_PARTIAL\"},\"finish_reason\":null}]}\n\n";

#[tokio::test]
async fn post_content_timeout_without_opt_in_retains_the_existing_partial_result() {
    // Omitted from the file and explicitly false are the same opt-out.
    for explicit_false in [false, true] {
        let project = Project::new();
        project.with_http_policy(1, 2, 10);
        // The shipped file already says false; the omitted case removes it so an
        // older file is exercised too.
        let path = project.workspace().join("config/orchestration.toml");
        let config = std::fs::read_to_string(&path).unwrap();
        let config = if explicit_false {
            config
        } else {
            config.replace("post_content_timeout_recovery = false\n", "")
        };
        assert_eq!(
            config.contains("post_content_timeout_recovery"),
            explicit_false,
            "the fixture did not set the case"
        );
        std::fs::write(&path, config).unwrap();
        let label = if explicit_false {
            "explicit-false"
        } else {
            "omitted"
        };
        let hold = Arc::new(tokio::sync::Notify::new());
        let fixture = fixture(vec![Reply::Script {
            steps: vec![
                Step::Write(LIVE_THINKING),
                Step::Write(LIVE_TEXT),
                Step::Write(ABANDONED_TOOL),
                Step::Signal(hold.clone()),
                Step::Hold,
            ],
        }])
        .await;
        Transport::BuiltinChat.configure(&project, &fixture.base);
        // The window closes well before the one-second idle deadline, so only a
        // live delta can be observed inside it. A build that staged output while
        // disabled flushes it at the terminal frame, after the deadline.
        let (status, stdout, frames, windowed, stderr) = run_streaming_frames(
            Transport::BuiltinChat.command(&project, SESSION_MARKER),
            hold,
            Duration::from_millis(400),
        )
        .await;
        let text = format!("{stdout}{stderr}");

        assert!(
            windowed
                .iter()
                .any(|frame| frame["type"] == "text" && frame["delta"] == "VISIBLE_PARTIAL"),
            "{label}: the text delta was not live while the body was open: {windowed:?}"
        );
        assert!(
            windowed
                .iter()
                .any(|frame| frame["type"] == "thinking" && frame["delta"] == "THINK_PARTIAL"),
            "{label}: the thinking delta was not live: {windowed:?}"
        );
        assert!(
            !windowed.iter().any(|frame| frame["type"] == "result"),
            "{label}: a result existed before the stream ended"
        );
        // One logical response start for the whole turn.
        assert_eq!(
            frames
                .iter()
                .filter(|(frame, _)| frame["type"] == "message_start")
                .count(),
            1,
            "{label}: {text}"
        );
        assert_eq!(status.code(), Some(1), "{label}: {text}");
        assert_eq!(
            fixture.count(),
            1,
            "{label}: a second POST was sent: {text}"
        );
        assert!(
            post_content_notices(&project.data(), SESSION_MARKER).is_empty(),
            "{label}: a recovery notice was emitted"
        );
        let results: Vec<&Value> = frames
            .iter()
            .map(|(frame, _)| frame)
            .filter(|frame| frame["type"] == "result")
            .collect();
        assert_eq!(results.len(), 1, "{label}: {text}");
        let result = results[0];
        assert_eq!(result["is_error"], true, "{label}: {result}");
        assert_eq!(result["stop_reason"], "stream_error", "{label}: {result}");
        let answer = result["result"].as_str().unwrap_or_default();
        assert_eq!(
            answer.matches("VISIBLE_PARTIAL").count(),
            1,
            "{label}: {answer}"
        );
        assert!(!answer.contains("THINK_PARTIAL"), "{label}: {answer}");
        // The persisted body and thinking match what the host saw, once each.
        let (stored, thinking) = stored_assistant(&project.data(), SESSION_MARKER);
        assert_eq!(
            stored.matches("VISIBLE_PARTIAL").count(),
            1,
            "{label}: {stored}"
        );
        assert_eq!(
            thinking.matches("THINK_PARTIAL").count(),
            1,
            "{label}: {thinking}"
        );
        // No tool ever started, ran or was recorded; the unfinished Write is gone.
        assert!(
            !frames
                .iter()
                .any(|(frame, _)| frame["type"] == "tool_start" || frame["type"] == "tool_result"),
            "{label}: a tool event was emitted: {text}"
        );
        assert!(
            tool_rows(&project.data(), SESSION_MARKER).is_empty(),
            "{label}: a tool row was stored"
        );
        assert!(
            !project.workspace().join("orphan.txt").exists(),
            "{label}: a partial tool call was dispatched"
        );
        let rows = turn_rows(&project.data(), SESSION_MARKER);
        assert_eq!(rows.len(), 1, "{label}: {rows:?}");
        assert_eq!(rows[0].1, "failed", "{label}: {rows:?}");
    }
}

#[tokio::test]
async fn post_content_recovery_does_not_retry_non_timeout_stream_failures() {
    // A clean end before the terminal marker is not a timeout, so it is final.
    {
        let project = Project::new();
        project.enable_post_content_recovery();
        project.with_http_policy(1, 2, 10);
        let fixture = fixture(vec![Reply::Script {
            steps: vec![Step::Write(LIVE_TEXT)],
        }])
        .await;
        Transport::BuiltinChat.configure(&project, &fixture.base);
        let output = execute(Transport::BuiltinChat.command(&project, SESSION_MARKER)).await;
        let text = combined_output(&output);
        assert_eq!(output.status.code(), Some(1), "{text}");
        assert_eq!(fixture.count(), 1, "a clean EOF was replayed: {text}");
        assert!(post_content_notices(&project.data(), SESSION_MARKER).is_empty());
        let frames = stream_frames(&output);
        let result = frames
            .iter()
            .find(|frame| frame["type"] == "result")
            .unwrap();
        assert_eq!(result["is_error"], true, "{result}");
        assert_eq!(
            result["result"]
                .as_str()
                .unwrap_or_default()
                .matches("VISIBLE_PARTIAL")
                .count(),
            1,
            "{result}"
        );
    }

    // A raw `[DONE]` before the stall is terminal even without a finish reason.
    {
        let project = Project::new();
        project.enable_post_content_recovery();
        project.with_http_policy(1, 2, 10);
        let fixture = fixture(vec![Reply::Script {
            steps: vec![
                Step::Write(LIVE_TEXT),
                Step::Write("data: [DONE]\n\n"),
                Step::Hold,
            ],
        }])
        .await;
        Transport::BuiltinChat.configure(&project, &fixture.base);
        let output = execute(Transport::BuiltinChat.command(&project, SESSION_MARKER)).await;
        let text = combined_output(&output);
        assert_eq!(output.status.code(), Some(1), "{text}");
        assert_eq!(fixture.count(), 1, "a terminal marker was replayed: {text}");
        assert!(
            post_content_notices(&project.data(), SESSION_MARKER).is_empty(),
            "a terminal marker authorised a replay"
        );
    }

    // A provider SSE error followed by a real body timeout is still terminal.
    {
        let project = Project::new();
        project.enable_post_content_recovery();
        project.with_http_policy(1, 2, 10);
        let fixture = fixture(vec![Reply::Script {
            steps: vec![
                Step::Write(LIVE_TEXT),
                Step::Write("data: {\"error\":{\"type\":\"invalid_request_error\",\"message\":\"fixture\"}}\n\n"),
                Step::Hold,
            ],
        }])
        .await;
        Transport::OpenAI.configure(&project, &fixture.base);
        let output = execute(Transport::OpenAI.command(&project, SESSION_MARKER)).await;
        let text = combined_output(&output);
        assert_eq!(output.status.code(), Some(1), "{text}");
        assert_eq!(fixture.count(), 1, "a provider error was replayed: {text}");
        assert!(
            post_content_notices(&project.data(), SESSION_MARKER).is_empty(),
            "a provider error authorised a replay"
        );
        let frames = stream_frames(&output);
        let result = frames
            .iter()
            .find(|frame| frame["type"] == "result")
            .unwrap();
        assert_eq!(result["is_error"], true, "{result}");
        assert!(result["result"]
            .as_str()
            .unwrap_or_default()
            .contains("VISIBLE_PARTIAL"));
    }

    // A finish reason on a non-leading choice is terminal too.
    {
        let project = Project::new();
        project.enable_post_content_recovery();
        project.with_http_policy(1, 2, 10);
        let fixture = fixture(vec![Reply::Script {
            steps: vec![
                Step::Write("data: {\"choices\":[{\"delta\":{\"content\":\"VISIBLE_PARTIAL\"},\"finish_reason\":null},{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n"),
                Step::Hold,
            ],
        }])
        .await;
        Transport::OpenAI.configure(&project, &fixture.base);
        let output = execute(Transport::OpenAI.command(&project, SESSION_MARKER)).await;
        let text = combined_output(&output);
        assert_eq!(output.status.code(), Some(1), "{text}");
        assert_eq!(
            fixture.count(),
            1,
            "a trailing finish reason was replayed: {text}"
        );
        assert!(
            post_content_notices(&project.data(), SESSION_MARKER).is_empty(),
            "a trailing finish reason authorised a replay"
        );
    }
}

const EMPTY_PRELUDE_EVENT: &str =
    "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n";

#[tokio::test]
async fn post_content_timeout_recovery_is_bounded_and_cancellable() {
    // 1. The replacement itself stalls after output: exactly two POSTs and a
    //    final failure, with the replacement's own text kept.
    {
        let project = Project::new();
        project.enable_post_content_recovery();
        project.with_http_policy(1, 2, 10);
        let fixture = fixture(vec![
            Reply::Script {
                steps: vec![Step::Write(LIVE_TEXT), Step::Hold],
            },
            Reply::Script {
                steps: vec![Step::Write("data: {\"choices\":[{\"delta\":{\"content\":\"SECOND_STALL\"},\"finish_reason\":null}]}\n\n"), Step::Hold],
            },
        ])
        .await;
        Transport::BuiltinChat.configure(&project, &fixture.base);
        let output = execute(Transport::BuiltinChat.command(&project, SESSION_MARKER)).await;
        let text = combined_output(&output);
        assert_eq!(output.status.code(), Some(1), "{text}");
        assert_eq!(fixture.count(), 2, "a third POST was sent: {text}");
        assert_eq!(
            post_content_notices(&project.data(), SESSION_MARKER).len(),
            1
        );
        let frames = stream_frames(&output);
        let result = frames
            .iter()
            .find(|frame| frame["type"] == "result")
            .unwrap();
        assert_eq!(result["is_error"], true, "{result}");
        // Only the final attempt's staged text is committed.
        assert_eq!(
            result["result"]
                .as_str()
                .unwrap_or_default()
                .matches("SECOND_STALL")
                .count(),
            1,
            "{result}"
        );
        assert!(!result["result"]
            .as_str()
            .unwrap_or_default()
            .contains("VISIBLE_PARTIAL"));
        assert_eq!(turn_status(&project.data(), SESSION_MARKER), "failed");
    }

    // 2. The replacement fails before its headers: no second recovery.
    {
        let project = Project::new();
        project.enable_post_content_recovery();
        project.with_http_policy(1, 2, 10);
        let fixture = fixture(vec![
            Reply::Script {
                steps: vec![Step::Write(LIVE_TEXT), Step::Hold],
            },
            Reply::CloseAfterRequest,
            Reply::Script {
                steps: vec![Step::Write(RECOVERED_TEXT)],
            },
        ])
        .await;
        Transport::BuiltinChat.configure(&project, &fixture.base);
        let output = execute(Transport::BuiltinChat.command(&project, SESSION_MARKER)).await;
        let text = combined_output(&output);
        assert_eq!(output.status.code(), Some(1), "{text}");
        assert_eq!(fixture.count(), 2, "a header failure was replayed: {text}");
        assert_eq!(
            post_content_notices(&project.data(), SESSION_MARKER).len(),
            1
        );
    }

    // 3. A later step stalls after a successful recovery: recovery is spent for
    //    the turn, so that later step fails without another POST.
    {
        let project = Project::new();
        project.enable_post_content_recovery();
        project.with_http_policy(1, 2, 10);
        let fixture = fixture(vec![
            Reply::Script {
                steps: vec![Step::Write(LIVE_TEXT), Step::Hold],
            },
            Reply::Script {
                steps: vec![
                    Step::Write(RECOVERY_TOOL_ROUND),
                    Step::Write(RECOVERY_TOOL_STOP),
                    Step::Write("data: [DONE]\n\n"),
                ],
            },
            Reply::Script {
                steps: vec![Step::Write("data: {\"choices\":[{\"delta\":{\"content\":\"THIRD_STALL\"},\"finish_reason\":null}]}\n\n"), Step::Hold],
            },
        ])
        .await;
        Transport::BuiltinChat.configure(&project, &fixture.base);
        let output = execute(Transport::BuiltinChat.command(&project, SESSION_MARKER)).await;
        let text = combined_output(&output);
        assert_eq!(output.status.code(), Some(1), "{text}");
        assert_eq!(fixture.count(), 3, "the turn recovered twice: {text}");
        assert_eq!(
            post_content_notices(&project.data(), SESSION_MARKER).len(),
            1,
            "more than one recovery notice"
        );
        // The committed tool still ran, once.
        assert_eq!(
            std::fs::read_to_string(project.workspace().join("marker.txt")).unwrap_or_default(),
            "one"
        );
    }

    // 4. A first leg that already spent the application budget on pre-content
    //    attempts cannot recover.
    {
        let project = Project::new();
        project.enable_post_content_recovery();
        // One retry: the empty prelude consumes the whole budget.
        project.with_http_policy(1, 1, 10);
        let fixture = fixture(vec![
            Reply::Script {
                steps: vec![Step::Write(EMPTY_PRELUDE_EVENT)],
            },
            Reply::Script {
                steps: vec![Step::Write(LIVE_TEXT), Step::Hold],
            },
        ])
        .await;
        Transport::BuiltinChat.configure(&project, &fixture.base);
        let output = execute(Transport::BuiltinChat.command(&project, SESSION_MARKER)).await;
        let text = combined_output(&output);
        assert_eq!(output.status.code(), Some(1), "{text}");
        assert_eq!(fixture.count(), 2, "a spent budget still replayed: {text}");
        assert!(
            post_content_notices(&project.data(), SESSION_MARKER).is_empty(),
            "a spent budget authorised a replay: {text}"
        );
    }

    // 5. Cancelling after the notice and before the replacement sends no POST.
    {
        const BACKOFF_MS: u64 = 2000;
        let project = Project::new();
        project.enable_post_content_recovery();
        project.with_http_policy(1, 2, BACKOFF_MS);
        let fixture = fixture(vec![
            Reply::Script {
                steps: vec![Step::Write(LIVE_TEXT), Step::Hold],
            },
            Reply::Script {
                steps: vec![Step::Write(RECOVERED_TEXT)],
            },
        ])
        .await;
        Transport::BuiltinChat.configure(&project, &fixture.base);
        let directory = worker_directory(&project.data(), SESSION_MARKER);
        let mut command = Transport::BuiltinChat.command(&project, SESSION_MARKER);
        command.env(
            deepseek_code_core::services::background_sessions::CONTROL_ENV,
            &directory,
        );
        let mut child = command.spawn().unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while post_content_notices(&project.data(), SESSION_MARKER).is_empty() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the recovery notice never appeared"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        std::fs::write(directory.join("cancel"), "").unwrap();
        let output = tokio::time::timeout(Duration::from_secs(30), child.wait_with_output())
            .await
            .expect("the CLI ignored the cancellation")
            .unwrap();
        let text = combined_output(&output);
        assert!(
            !output.status.success(),
            "a cancelled turn must not exit zero"
        );
        tokio::time::sleep(Duration::from_millis(BACKOFF_MS + 800)).await;
        assert_eq!(
            fixture.count(),
            1,
            "a cancelled recovery wait sent a replacement: {text}"
        );
        let frames = stream_frames(&output);
        let results: Vec<&Value> = frames
            .iter()
            .filter(|frame| frame["type"] == "result")
            .collect();
        assert_eq!(results.len(), 1, "{text}");
        assert_eq!(turn_status(&project.data(), SESSION_MARKER), "cancelled");
    }

    // 6. Cancelling while the replacement body is open issues no further POST
    //    and runs no tool.
    {
        let project = Project::new();
        project.enable_post_content_recovery();
        project.with_http_policy(5, 2, 10);
        let ready = Arc::new(tokio::sync::Notify::new());
        let fixture = fixture(vec![
            Reply::Script {
                steps: vec![Step::Write(LIVE_TEXT), Step::Hold],
            },
            Reply::Script {
                steps: vec![
                    Step::Write("data: {\"choices\":[{\"delta\":{\"content\":\"SECOND_OPEN\"},\"finish_reason\":null}]}\n\n"),
                    Step::Write("data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"SECOND_THINK\"},\"finish_reason\":null}]}\n\n"),
                    Step::Write(ABANDONED_TOOL),
                    // Headers and the partial body have flushed.
                    Step::Signal(ready.clone()),
                    Step::Hold,
                ],
            },
        ])
        .await;
        Transport::BuiltinChat.configure(&project, &fixture.base);
        let directory = worker_directory(&project.data(), SESSION_MARKER);
        let mut command = Transport::BuiltinChat.command(&project, SESSION_MARKER);
        command.env(
            deepseek_code_core::services::background_sessions::CONTROL_ENV,
            &directory,
        );
        let mut child = command.spawn().unwrap();
        let mut stdout = tokio::io::BufReader::new(child.stdout.take().unwrap());
        let mut stderr = child.stderr.take().unwrap();
        let drain = tokio::spawn(async move {
            let mut out = Vec::new();
            let _ = stderr.read_to_end(&mut out).await;
            String::from_utf8_lossy(&out).into_owned()
        });
        let frames: Arc<std::sync::Mutex<Vec<Value>>> = Default::default();
        let collected = frames.clone();
        let reader = tokio::spawn(async move {
            let mut line = String::new();
            loop {
                line.clear();
                match stdout.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if let Ok(value) = serde_json::from_str::<Value>(line.trim()) {
                            collected.lock().unwrap().push(value);
                        }
                    }
                }
            }
        });
        tokio::time::timeout(Duration::from_secs(20), ready.notified())
            .await
            .expect("the replacement body never flushed");
        std::fs::write(directory.join("cancel"), "").unwrap();
        let status = tokio::time::timeout(Duration::from_secs(30), child.wait())
            .await
            .expect("the CLI ignored the cancellation")
            .unwrap();
        let _ = reader.await;
        let stderr = drain.await.unwrap_or_default();
        assert!(!status.success(), "a cancelled turn must not exit zero");
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(fixture.count(), 2, "a cancellation POSTed again");
        assert!(
            !project.workspace().join("orphan.txt").exists(),
            "a cancelled replacement dispatched its pending tool"
        );
        // One terminal result, and the replacement's uncommitted output is gone.
        let frames = frames.lock().unwrap();
        let results: Vec<&Value> = frames
            .iter()
            .filter(|frame| frame["type"] == "result")
            .collect();
        assert_eq!(results.len(), 1, "one terminal result: {stderr}");
        let (stored, thinking) = stored_assistant(&project.data(), SESSION_MARKER);
        assert!(!stored.contains("SECOND_OPEN"), "{stored}");
        assert!(!thinking.contains("SECOND_THINK"), "{thinking}");
        assert!(!stored.contains("VISIBLE_PARTIAL"), "{stored}");
        assert!(
            !frames
                .iter()
                .any(|frame| frame["type"] == "tool_start" || frame["type"] == "tool_result"),
            "a cancelled replacement emitted a tool event"
        );
        assert!(
            tool_rows(&project.data(), SESSION_MARKER).is_empty(),
            "a cancelled replacement stored a tool row"
        );
        assert_eq!(
            turn_status(&project.data(), SESSION_MARKER),
            "cancelled",
            "the terminal persistence state"
        );
    }
}

/// Run the CLI reading stdout frame by frame, so a delta can be observed at a
/// known moment. `hold` fires once the fixture has flushed its script and is
/// holding the body open; `window` is then read before letting the run finish,
/// and must stay below the configured idle deadline.
async fn run_streaming_frames(
    mut command: Command,
    hold: Arc<tokio::sync::Notify>,
    window: Duration,
) -> (
    std::process::ExitStatus,
    String,
    Vec<(Value, std::time::Instant)>,
    Vec<Value>,
    String,
) {
    let mut child = command.spawn().unwrap();
    let mut stdout = tokio::io::BufReader::new(child.stdout.take().unwrap());
    let mut stderr = child.stderr.take().unwrap();
    let drain = tokio::spawn(async move {
        let mut out = Vec::new();
        let _ = stderr.read_to_end(&mut out).await;
        out
    });
    tokio::time::timeout(Duration::from_secs(20), hold.notified())
        .await
        .expect("the fixture never flushed its script");
    let mut raw = String::new();
    let mut frames = Vec::new();
    // Frames read before the window closed, i.e. while the body was still held.
    let mut windowed = Vec::new();
    let mut observed = false;
    let deadline = std::time::Instant::now() + window;
    let mut line = String::new();
    loop {
        line.clear();
        let remaining = if observed {
            Duration::from_secs(30)
        } else {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                observed = true;
                continue;
            }
            left
        };
        match tokio::time::timeout(remaining, stdout.read_line(&mut line)).await {
            Ok(Ok(0)) | Ok(Err(_)) => break,
            Ok(Ok(_)) => {}
            // The observation window closed; keep reading to the end of the run.
            Err(_) if !observed => {
                observed = true;
                continue;
            }
            Err(_) => break,
        }
        raw.push_str(&line);
        if let Ok(value) = serde_json::from_str::<Value>(line.trim()) {
            if !observed {
                windowed.push(value.clone());
            }
            frames.push((value, std::time::Instant::now()));
        }
    }
    let status = child.wait().await.unwrap();
    let stderr = String::from_utf8_lossy(&drain.await.unwrap_or_default()).into_owned();
    (status, raw, frames, windowed, stderr)
}

/// The most recent stored assistant body and thinking for a session.
fn stored_assistant(data: &Path, session: &str) -> (String, String) {
    let paths = deepseek_code_core::runtime::RuntimePaths::new(data.to_path_buf()).unwrap();
    let db = paths.open_database().unwrap();
    db.shard_sql(session, |conn| -> anyhow::Result<(String, String)> {
        Ok(conn.query_row(
            "SELECT content, COALESCE(thinking,'') FROM messages WHERE conversation_id=?1 AND role='assistant' ORDER BY sort_order DESC LIMIT 1",
            [session],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?)
    })
    .unwrap()
    .unwrap()
}

/// Every persisted tool row for a session, as (name, input).
fn tool_rows(data: &Path, session: &str) -> Vec<(String, String)> {
    let paths = deepseek_code_core::runtime::RuntimePaths::new(data.to_path_buf()).unwrap();
    let db = paths.open_database().unwrap();
    db.shard_sql(session, |conn| -> anyhow::Result<Vec<(String, String)>> {
        let mut statement = conn.prepare(
            "SELECT tc.name, COALESCE(tc.input,'') FROM tool_calls tc JOIN messages m ON m.id=tc.message_id WHERE m.conversation_id=?1 ORDER BY m.sort_order, tc.sort_order",
        )?;
        let mut rows = statement.query([session])?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            out.push((row.get(0)?, row.get(1)?));
        }
        Ok(out)
    })
    .unwrap()
    .unwrap()
}

/// Frames of one stream-json type, in order.
fn frames_of<'a>(frames: &'a [(Value, std::time::Instant)], kind: &str) -> Vec<&'a Value> {
    frames
        .iter()
        .filter(|(frame, _)| frame["type"] == kind)
        .map(|(frame, _)| frame)
        .collect()
}

const NESTED_PROVIDER_ERROR: &str =
    "data: {\"error\":{\"type\":\"overloaded_error\",\"message\":\"busy\"}}\n\n";
const TOP_LEVEL_PROVIDER_ERROR: &str = "data: {\"type\":\"error\",\"message\":\"busy\"}\n\n";
const WAITING_SUCCESS: &str = "data: {\"id\":\"waiting\",\"model\":\"fixture\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"REPLAYED_BY_MISTAKE\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

#[tokio::test]
async fn post_content_recovery_vetoes_top_level_and_nested_provider_errors() {
    for (label, error_event) in [
        ("nested", NESTED_PROVIDER_ERROR),
        ("top-level", TOP_LEVEL_PROVIDER_ERROR),
    ] {
        let project = Project::new();
        project.enable_post_content_recovery();
        project.with_http_policy(1, 2, 10);
        let fixture = fixture(vec![
            Reply::Script {
                steps: vec![Step::Write(LIVE_TEXT), Step::Write(error_event), Step::Hold],
            },
            // A valid replacement is waiting: any second POST would consume it.
            Reply::Script {
                steps: vec![Step::Write(WAITING_SUCCESS)],
            },
        ])
        .await;
        // Direct Chat forwards whichever shape the provider sends.
        Transport::OpenAI.configure(&project, &fixture.base);
        let output = execute(Transport::OpenAI.command(&project, SESSION_MARKER)).await;
        let text = combined_output(&output);

        assert_eq!(output.status.code(), Some(1), "{label}: {text}");
        assert_eq!(
            fixture.count(),
            1,
            "{label}: a provider error was replayed: {text}"
        );
        assert!(
            post_content_notices(&project.data(), SESSION_MARKER).is_empty(),
            "{label}: a provider error authorised a replay"
        );
        assert!(
            !text.contains("REPLAYED_BY_MISTAKE"),
            "{label}: the waiting replacement was consumed: {text}"
        );
        let frames = stream_frames(&output);
        let results: Vec<&Value> = frames
            .iter()
            .filter(|frame| frame["type"] == "result")
            .collect();
        assert_eq!(results.len(), 1, "{label}: {text}");
        assert_eq!(results[0]["is_error"], true, "{label}: {}", results[0]);
        assert_eq!(
            results[0]["stop_reason"], "stream_error",
            "{label}: {}",
            results[0]
        );
        assert_eq!(
            results[0]["result"]
                .as_str()
                .unwrap_or_default()
                .matches("VISIBLE_PARTIAL")
                .count(),
            1,
            "{label}: {text}"
        );
        assert_eq!(turn_status(&project.data(), SESSION_MARKER), "failed");
    }
}

const LATER_TEXT_EVENT: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null},{\"index\":1,\"delta\":{\"content\":\"LATER_TEXT\"},\"finish_reason\":null}]}\n\n";
const LATER_THINKING_EVENT: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null},{\"index\":1,\"delta\":{\"reasoning_content\":\"LATER_THINK\"},\"finish_reason\":null}]}\n\n";
const LATER_TOOL_EVENT: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null},{\"index\":1,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-orphan\",\"type\":\"function\",\"function\":{\"name\":\"Write\",\"arguments\":\"{\\\"file_path\\\":\\\"orphan.txt\\\"\"}}]},\"finish_reason\":null}]}\n\n";
const PRELUDE_ONLY_EVENT: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null},{\"index\":1,\"delta\":{\"content\":\"\"},\"finish_reason\":null}]}\n\n";
const EMPTY_CHOICES_EVENT: &str = "data: {\"choices\":[]}\n\n";

#[tokio::test]
async fn post_content_recovery_counts_content_on_non_leading_choices() {
    // The loop consumes text, thinking and tool deltas from every choice.
    for (label, stalled, marker) in [
        ("later-text", LATER_TEXT_EVENT, "LATER_TEXT"),
        ("later-thinking", LATER_THINKING_EVENT, "LATER_THINK"),
        ("later-tool", LATER_TOOL_EVENT, "call-orphan"),
    ] {
        let project = Project::new();
        project.enable_post_content_recovery();
        project.with_http_policy(1, 2, 10);
        let bodies: Arc<std::sync::Mutex<Vec<Vec<u8>>>> = Default::default();
        let posts: Arc<std::sync::Mutex<Vec<std::time::Instant>>> = Default::default();
        let fixture = fixture(vec![
            Reply::RecordScript {
                steps: vec![Step::Write(stalled), Step::Hold],
                bodies: bodies.clone(),
                posts: posts.clone(),
            },
            Reply::RecordScript {
                steps: vec![
                    Step::Write(RECOVERED_TEXT),
                    Step::Write(RECOVERED_STOP),
                    Step::Write("data: [DONE]\n\n"),
                ],
                bodies: bodies.clone(),
                posts: posts.clone(),
            },
        ])
        .await;
        Transport::OpenAI.configure(&project, &fixture.base);
        let output = execute(Transport::OpenAI.command(&project, SESSION_MARKER)).await;
        let text = combined_output(&output);

        assert!(output.status.success(), "{label}: {text}");
        assert_eq!(fixture.count(), 2, "{label}: {text}");
        let frames = stream_frames(&output);
        let results: Vec<&Value> = frames
            .iter()
            .filter(|frame| frame["type"] == "result")
            .collect();
        assert_eq!(results.len(), 1, "{label}: {text}");
        assert_eq!(results[0]["is_error"], false, "{label}: {}", results[0]);
        assert_eq!(
            results[0]["result"]
                .as_str()
                .unwrap_or_default()
                .matches("recovered answer")
                .count(),
            1,
            "{label}: {text}"
        );
        assert!(
            frames
                .iter()
                .filter(|frame| frame["type"] == "message_start")
                .count()
                == 1,
            "{label}: the replacement announced a second logical start"
        );
        // One notice, naming the stalled leg.
        let notices = post_content_notices(&project.data(), SESSION_MARKER);
        assert_eq!(notices.len(), 1, "{label}: {notices:?}");
        let snapshots = persisted_snapshots(&project.data(), SESSION_MARKER);
        let step_one: Vec<&Value> = snapshots
            .iter()
            .filter(|snapshot| snapshot["number"] == 1)
            .collect();
        assert_eq!(step_one.len(), 2, "{label}: {step_one:?}");
        let abandoned = step_one
            .iter()
            .find(|snapshot| snapshot["abandonedForRecovery"] == true)
            .unwrap_or_else(|| panic!("{label}: no abandoned snapshot"));
        let replacement = step_one
            .iter()
            .find(|snapshot| snapshot["abandonedForRecovery"] != true)
            .unwrap();
        assert_eq!(notices[0]["requestId"], abandoned["id"], "{label}");
        assert_eq!(replacement["replacesRequestId"], abandoned["id"], "{label}");
        assert_ne!(replacement["id"], abandoned["id"], "{label}");
        // Both legs carried the same prepared conversation.
        let recorded = bodies.lock().unwrap();
        assert_eq!(recorded.len(), 2, "{label}");
        let first = request_body(&recorded[0]);
        let second = request_body(&recorded[1]);
        assert_eq!(first["messages"], second["messages"], "{label}");
        // Nothing abandoned reaches the host, the context or the transcript.
        for needle in [marker, "LATER_"] {
            assert!(!text.contains(needle), "{label}: {needle} leaked: {text}");
            assert!(
                !second["messages"].to_string().contains(needle),
                "{label}: {needle} reached the replacement context"
            );
        }
        let (stored, thinking) = stored_assistant(&project.data(), SESSION_MARKER);
        for needle in [marker, "LATER_"] {
            assert!(!stored.contains(needle), "{label}: {stored}");
            assert!(!thinking.contains(needle), "{label}: {thinking}");
        }
        assert!(!project.workspace().join("orphan.txt").exists(), "{label}");
        assert!(
            !tool_rows(&project.data(), SESSION_MARKER)
                .iter()
                .any(|(_, input)| input.contains("orphan")),
            "{label}: the abandoned call was stored"
        );
        assert_eq!(
            turn_status(&project.data(), SESSION_MARKER),
            "completed",
            "{label}"
        );
    }

    // A frame with only prelude content is not content: A retries it under the
    // same request ID, and C never notices.
    {
        let project = Project::new();
        project.enable_post_content_recovery();
        project.with_http_policy(1, 2, 10);
        let fixture = fixture(vec![
            Reply::Script {
                steps: vec![
                    Step::Write(PRELUDE_ONLY_EVENT),
                    Step::Write(EMPTY_CHOICES_EVENT),
                ],
            },
            Reply::Script {
                steps: vec![
                    Step::Write(RECOVERED_TEXT),
                    Step::Write(RECOVERED_STOP),
                    Step::Write("data: [DONE]\n\n"),
                ],
            },
        ])
        .await;
        Transport::OpenAI.configure(&project, &fixture.base);
        let output = execute(Transport::OpenAI.command(&project, SESSION_MARKER)).await;
        let text = combined_output(&output);
        assert!(output.status.success(), "prelude: {text}");
        assert_eq!(fixture.count(), 2, "prelude: {text}");
        assert!(
            post_content_notices(&project.data(), SESSION_MARKER).is_empty(),
            "prelude: C handled a pre-content retry"
        );
        let events = observed_attempts(&project.data(), SESSION_MARKER);
        assert!(
            events.iter().any(|event| event["outcome"] == "retry_wait"),
            "prelude: A did not retry: {events:?}"
        );
        let ids: std::collections::BTreeSet<&str> = events
            .iter()
            .filter_map(|event| event["requestId"].as_str())
            .collect();
        assert_eq!(ids.len(), 1, "prelude: the retry changed identity: {ids:?}");
    }
}

const MIXED_CHOICES: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Before \"},\"finish_reason\":\"stop\"},{\"index\":1,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"fixture-mixed\",\"type\":\"function\",\"function\":{\"name\":\"Bash\",\"arguments\":\"{\\\"command\\\":\\\"printf one >> marker.txt\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\n";
/// Step two carries `model` (so `responseModel` appears) and no finish reason.
const MIXED_FOLLOWUP: &str = "data: {\"id\":\"mock-2\",\"model\":\"fixture-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"After\"},\"finish_reason\":null}]}\n\n";

/// The raw `message_trajectory.requests_json` entries for a session, in message
/// and request order. The stored form is what the differential pins, so it is
/// read without hydrating the externalized fields.
fn all_snapshots(data: &Path, session: &str) -> Vec<Value> {
    let paths = deepseek_code_core::runtime::RuntimePaths::new(data.to_path_buf()).unwrap();
    let db = paths.open_database().unwrap();
    db.shard_sql(session, |conn| -> anyhow::Result<Vec<Value>> {
        let mut statement = conn.prepare(
            "SELECT mt.requests_json FROM message_trajectory mt JOIN messages m ON m.id=mt.message_id WHERE m.conversation_id=?1 AND m.role='assistant' ORDER BY m.sort_order",
        )?;
        let rows: Vec<String> = statement
            .query_map([session], |row| row.get::<_, String>(0))?
            .collect::<Result<_, _>>()?;
        let mut out = Vec::new();
        for raw in rows {
            let value: Value = serde_json::from_str(&raw)?;
            if let Some(list) = value.as_array() {
                out.extend(list.iter().cloned());
            }
        }
        Ok(out)
    })
    .unwrap()
    .unwrap()
}

/// The pre-C snapshot key set, pinned from the round-04 control.
const PRE_C_SNAPSHOT_KEYS: &[&str] = &[
    "apiFormat",
    "cacheReadTokens",
    "cacheWriteTokens",
    "contentTokens",
    "contextBoundary",
    "contextUsage",
    "contextWindow",
    "effort",
    "endedAt",
    "error",
    "finishReason",
    "firstTokenAt",
    "generationMs",
    "id",
    "inputTokens",
    "maxOutputTokens",
    "model",
    "number",
    "outputTokens",
    "parentSessionId",
    "providerId",
    "providerName",
    "reasoningTokens",
    "reservedForCompletion",
    "responseBlocks",
    "responseId",
    "sessionId",
    "source",
    "startedAt",
    "status",
    "systemMessagesRef",
    "systemPromptRef",
    "threadId",
    "throughputTokensPerSecond",
    "timingSource",
    "toolsRef",
    "totalDurationMs",
    "totalTokens",
    "ttftMs",
    "turnId",
    "usage",
];

#[tokio::test]
async fn post_content_disabled_matches_pre_c_snapshot_keys_and_stop_reason() {
    // Omitted and explicitly false must both match the round-04 control.
    for explicit_false in [false, true] {
        let label = if explicit_false {
            "explicit-false"
        } else {
            "omitted"
        };
        let project = Project::new();
        project.with_http_policy(1, 2, 10);
        let path = project.workspace().join("config/orchestration.toml");
        let config = std::fs::read_to_string(&path).unwrap();
        let config = if explicit_false {
            config
        } else {
            config.replace("post_content_timeout_recovery = false\n", "")
        };
        std::fs::write(&path, config).unwrap();
        let fixture = fixture(vec![
            Reply::Script {
                steps: vec![Step::Write(MIXED_CHOICES), Step::Write("data: [DONE]\n\n")],
            },
            Reply::Script {
                steps: vec![Step::Write(MIXED_FOLLOWUP), Step::Write("data: [DONE]\n\n")],
            },
        ])
        .await;
        Transport::OpenAI.configure(&project, &fixture.base);
        let output = execute(Transport::OpenAI.command(&project, SESSION_MARKER)).await;
        let text = combined_output(&output);

        assert!(output.status.success(), "{label}: {text}");
        assert_eq!(fixture.count(), 2, "{label}: {text}");
        assert_eq!(
            std::fs::read_to_string(project.workspace().join("marker.txt")).unwrap_or_default(),
            "one",
            "{label}: the tool did not run exactly once"
        );
        let frames = stream_frames(&output);
        let results: Vec<&Value> = frames
            .iter()
            .filter(|frame| frame["type"] == "result")
            .collect();
        assert_eq!(results.len(), 1, "{label}: {text}");
        assert_eq!(results[0]["is_error"], false, "{label}: {}", results[0]);
        assert_eq!(
            results[0]["stop_reason"], "stop",
            "{label}: the initial-leg stop reason was not preserved: {}",
            results[0]
        );
        assert_eq!(
            results[0]["result"], "Before \n\nAfter",
            "{label}: {}",
            results[0]
        );

        let snapshots = all_snapshots(&project.data(), SESSION_MARKER);
        assert_eq!(snapshots.len(), 2, "{label}: {snapshots:?}");
        assert_eq!(snapshots[0]["number"], 1, "{label}");
        assert_eq!(snapshots[1]["number"], 2, "{label}");
        for (index, snapshot) in snapshots.iter().enumerate() {
            let keys: std::collections::BTreeSet<&str> = snapshot
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            let mut expected: std::collections::BTreeSet<&str> =
                PRE_C_SNAPSHOT_KEYS.iter().copied().collect();
            if snapshot["number"] == 2 {
                expected.insert("responseModel");
            }
            assert_eq!(
                keys, expected,
                "{label}: snapshot {index} key set changed: {snapshot:?}"
            );
            assert!(
                !keys.contains("replacesRequestId") && !keys.contains("abandonedForRecovery"),
                "{label}: a recovery-only key reached the disabled path: {snapshot:?}"
            );
        }
    }
}
