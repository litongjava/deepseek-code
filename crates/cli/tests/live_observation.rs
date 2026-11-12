//! Live state of a headless (direct print/stream-json) turn.
//!
//! These tests run the real CLI binary: one process owns the turn while another
//! observes the same session. They check the durable publication of intermediate
//! progress, settlement after the owner dies, and that observing never requests
//! a model response or re-executes a tool.
use deepseek_code_core::{
    db::{message_repo, turn_repo, DbManager},
    runtime::RuntimePaths,
};
use serde_json::{json, Value};
use std::{
    process::Stdio,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    process::Command,
};

struct Mock {
    url: String,
    requests: Arc<AtomicUsize>,
}

fn sse(frames: impl IntoIterator<Item = Value>) -> String {
    let mut stream: String = frames
        .into_iter()
        .map(|frame| format!("data: {frame}\n\n"))
        .collect();
    stream.push_str("data: [DONE]\n\n");
    stream
}

fn tool_turn() -> String {
    sse([
        json!({"model":"fixture-model","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"read-1","type":"function","function":{"name":"Read","arguments":"{\"file_path\":\"fixture.txt\"}"}}]},"finish_reason":null}]}),
        json!({"model":"fixture-model","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":"tool_calls"}]}),
    ])
}

fn text_turn(chunks: usize) -> String {
    let mut frames = vec![
        json!({"model":"fixture-model","choices":[{"index":0,"delta":{"role":"assistant","content":"final answer"},"finish_reason":null}]}),
    ];
    for index in 0..chunks {
        frames.push(json!({"model":"fixture-model","choices":[{"index":0,"delta":{"role":"assistant","content":format!(" streamed{index}")},"finish_reason":null}]}));
    }
    frames.push(json!({"model":"fixture-model","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":"stop"}],"usage":{"prompt_tokens":30,"completion_tokens":12,"total_tokens":42}}));
    sse(frames)
}

async fn read_request(socket: &mut tokio::net::TcpStream) {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let read = socket.read(&mut chunk).await.unwrap_or(0);
        if read == 0 {
            return;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buffer[..end]).to_ascii_lowercase();
            let length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            while buffer.len() < end + 4 + length {
                let read = socket.read(&mut chunk).await.unwrap_or(0);
                if read == 0 {
                    return;
                }
                buffer.extend_from_slice(&chunk[..read]);
            }
            return;
        }
    }
}

impl Mock {
    /// The first response asks for a real `Read`; later responses stream text
    /// one chunk per `delay` so progress is observable while the turn runs.
    async fn start(delay: Duration, chunks: usize) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let seen = requests.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let seen = seen.clone();
                tokio::spawn(async move {
                    read_request(&mut socket).await;
                    let index = seen.fetch_add(1, Ordering::SeqCst);
                    let _ = socket
                        .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n")
                        .await;
                    if index == 0 {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        let _ = socket.write_all(tool_turn().as_bytes()).await;
                    } else {
                        let (count, pause) = if index == 1 {
                            (chunks, delay)
                        } else {
                            (3, Duration::from_millis(10))
                        };
                        let stream = text_turn(count);
                        for frame in stream.split_inclusive("\n\n") {
                            tokio::time::sleep(pause).await;
                            if socket.write_all(frame.as_bytes()).await.is_err() {
                                return;
                            }
                        }
                    }
                    let _ = socket.shutdown().await;
                });
            }
        });
        Self { url, requests }
    }

    fn count(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

struct Project {
    temp: tempfile::TempDir,
    db: Arc<DbManager>,
}

impl Project {
    fn new(url: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let workspace = temp.path().join("project");
        std::fs::create_dir_all(data.join("config")).unwrap();
        std::fs::create_dir_all(workspace.join("config")).unwrap();
        std::fs::write(workspace.join("fixture.txt"), "fixture contents\n").unwrap();
        std::fs::write(
            data.join("providers.json"),
            json!([{
                "id":"fixture","name":"Fixture","base_url":url,"api_key":"fixture-only",
                "api_format":"openai","enabled":true,
                "models":[{"id":"fixture-model","name":"Fixture","enabled":true,
                    "context_window":128000,"max_tokens":4096,
                    "supports_web_search":false,"supports_vision":false}]
            }])
            .to_string(),
        )
        .unwrap();
        // Keep retries out of the timing assertions; these fixtures never fail.
        std::fs::write(
            workspace.join("config/orchestration.toml"),
            include_str!("../../../src-tauri/config/orchestration.toml")
                .replace("retry_base_backoff_ms = 2000", "retry_base_backoff_ms = 10"),
        )
        .unwrap();
        let db = RuntimePaths::new(data).unwrap().open_database().unwrap();
        Self { temp, db }
    }

    fn data(&self) -> std::path::PathBuf {
        self.temp.path().join("data")
    }

    fn workspace(&self) -> std::path::PathBuf {
        self.temp.path().join("project")
    }

    fn assistant(&self, id: &str) -> Option<(String, String, usize, bool)> {
        let rows = self
            .db
            .with_conn(|conn| message_repo::get_messages_by_conversation(conn, id))
            .ok()?
            .ok()?;
        let row = rows.iter().rev().find(|row| row.role == "assistant")?;
        let tools = self
            .db
            .with_conn(|conn| message_repo::get_tool_calls_for_message(conn, &row.id))
            .ok()?
            .ok()?;
        let completed = tools.iter().any(|tool| tool.output.is_some());
        Some((row.id.clone(), row.content.clone(), tools.len(), completed))
    }
}

fn strip_internal_env(command: &mut Command) {
    for (key, _) in std::env::vars() {
        if key.starts_with("DEEPSEEK_CODE") || key.starts_with("CLAUDE_CODE") {
            command.env_remove(key);
        }
    }
}

fn owner(project: &Project, id: &str, resume: bool) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dcl"));
    command.args([
        "--print",
        "--no-interactive",
        "--output-format",
        "stream-json",
        "--data-dir",
    ]);
    command.arg(project.data());
    command.arg("--path").arg(project.workspace());
    command.args([
        "--provider",
        "fixture",
        "--model",
        "fixture-model",
        "--permission-mode",
        "ask",
    ]);
    if resume {
        command.args(["--resume", id, "continue explicitly"]);
    } else {
        command.args(["--session-id", id, "read the fixture and answer"]);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    strip_internal_env(&mut command);
    command
}

/// Opens the session exactly like a resume and submits no model request.
async fn resume_without_a_turn(project: &Project, id: &str) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dcl"));
    command.arg("--data-dir").arg(project.data());
    command.arg("--path").arg(project.workspace());
    command.args([
        "--resume",
        id,
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--print",
        "--permission-mode",
        "accept-edits",
        "--provider",
        "fixture",
        "--model",
        "fixture-model",
    ]);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    strip_internal_env(&mut command);
    let mut child = command.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let _ = stdin
        .write_all(b"{\"type\":\"control_request\",\"request_id\":\"init\",\"request\":{\"subtype\":\"initialize\"}}\n")
        .await;
    // Give the process time to answer the control request, then end its input.
    tokio::time::sleep(Duration::from_millis(500)).await;
    drop(stdin);
    tokio::time::timeout(Duration::from_secs(20), child.wait_with_output())
        .await
        .expect("the resuming process must exit")
        .unwrap()
}

async fn wait_until(
    project: &Project,
    id: &str,
    check: impl Fn(&(String, String, usize, bool)) -> bool,
) -> bool {
    let deadline = Instant::now() + Duration::from_secs(25);
    while Instant::now() < deadline {
        if let Some(progress) = project.assistant(id) {
            if check(&progress) {
                return true;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

#[tokio::test]
async fn a_headless_turn_publishes_tool_and_text_progress_before_it_finishes() {
    let mock = Mock::start(Duration::from_millis(250), 24).await;
    let project = Project::new(&mock.url);
    let id = uuid::Uuid::new_v4().to_string();
    let mut child = owner(&project, &id, false).spawn().unwrap();

    let mut published = None;
    let deadline = Instant::now() + Duration::from_secs(25);
    while Instant::now() < deadline {
        if let Some(progress) = project.assistant(&id) {
            assert!(
                child.try_wait().unwrap().is_none(),
                "progress must be published while the owner still runs"
            );
            if progress.2 > 0 && progress.1.contains("streamed") {
                published = Some(progress);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let (mid_id, mid_text, tool_count, _) =
        published.expect("the running turn never published its tool call and text");
    assert!(tool_count > 0 && mid_text.contains("streamed"));
    assert!(
        child.try_wait().unwrap().is_none(),
        "the turn must still be live when its progress is visible"
    );

    let output = tokio::time::timeout(Duration::from_secs(90), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let rows = project
        .db
        .with_conn(|conn| message_repo::get_messages_by_conversation(conn, &id))
        .unwrap()
        .unwrap();
    let assistant = rows
        .iter()
        .rev()
        .find(|row| row.role == "assistant")
        .unwrap();
    assert_eq!(
        assistant.id, mid_id,
        "finalization keeps the published message ID"
    );
    assert!(assistant.content.contains("final answer"));
    assert!(assistant.content.contains("streamed23"));
    let tools = project
        .db
        .with_conn(|conn| message_repo::get_tool_calls_for_message(conn, &assistant.id))
        .unwrap()
        .unwrap();
    assert_eq!(tools.len(), 1);
    assert!(tools[0]
        .output
        .as_deref()
        .is_some_and(|output| output.contains("fixture contents")));
    let turn = project
        .db
        .with_conn(|conn| turn_repo::get_message_turn(conn, &assistant.id))
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(turn.status, turn_repo::TurnStatus::Completed);
    assert!(
        turn.elapsed_ms.unwrap_or(0) >= 2_000,
        "duration is measured: {turn:?}"
    );
    assert_eq!(mock.count(), 2, "the tool loop made exactly two requests");
}

#[tokio::test]
async fn a_killed_owner_is_settled_without_replay_and_only_an_explicit_continuation_runs() {
    let mock = Mock::start(Duration::from_millis(500), 240).await;
    let project = Project::new(&mock.url);
    let id = uuid::Uuid::new_v4().to_string();
    let mut child = owner(&project, &id, false).spawn().unwrap();
    assert!(
        wait_until(&project, &id, |progress| progress.2 > 0
            && !progress.1.is_empty())
        .await,
        "the live turn must publish progress before it is killed"
    );
    child.start_kill().unwrap();
    let _ = child.wait().await;

    let running = project
        .db
        .with_conn(|conn| turn_repo::running_turn(conn, &id))
        .unwrap()
        .unwrap()
        .expect("the killed turn is still running in the database");
    assert_eq!(running.elapsed_ms, None);

    let requests_before_resume = mock.count();
    let output = resume_without_a_turn(&project, &id).await;
    assert!(
        output.status.success(),
        "resume without a turn must exit cleanly: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let settled = project
        .db
        .with_conn(|conn| turn_repo::latest_turn(conn, &id))
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        settled.interrupted(),
        "the dead owner is settled: {settled:?}"
    );
    assert_eq!(settled.ended_at, None);
    assert_eq!(
        mock.count(),
        requests_before_resume,
        "opening the session must not replay the request"
    );

    let output = tokio::time::timeout(Duration::from_secs(60), owner(&project, &id, true).output())
        .await
        .expect("the explicit continuation must finish")
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        mock.count(),
        requests_before_resume + 1,
        "only the explicit continuation sends a request"
    );
    let latest = project
        .db
        .with_conn(|conn| turn_repo::latest_turn(conn, &id))
        .unwrap()
        .unwrap()
        .unwrap();
    assert_ne!(latest.turn_id, settled.turn_id);
    assert_eq!(latest.status, turn_repo::TurnStatus::Completed);
    assert!(latest.elapsed_ms.is_some());
    // The killed call is not executed a second time.
    let rows = project
        .db
        .with_conn(|conn| message_repo::get_messages_by_conversation(conn, &id))
        .unwrap()
        .unwrap();
    let mut reads = 0;
    for row in rows.iter().filter(|row| row.role == "assistant") {
        let tools = project
            .db
            .with_conn(|conn| message_repo::get_tool_calls_for_message(conn, &row.id))
            .unwrap()
            .unwrap();
        reads += tools.iter().filter(|tool| tool.name == "Read").count();
    }
    assert_eq!(reads, 1, "the killed Read was not executed again");
}

/// A compaction hook is slow work that runs after the failed attempt's engine
/// call returned. Ownership must span it, or an observer would see an idle gap
/// and settle a live turn as interrupted.
#[tokio::test]
async fn ownership_survives_a_delayed_post_compact_hook() {
    use axum::{extract::State, response::IntoResponse, routing::post, Json, Router};

    #[derive(Clone)]
    struct Fixture {
        main: Arc<AtomicUsize>,
        marker: std::path::PathBuf,
    }

    async fn respond(
        State(state): State<Fixture>,
        Json(body): Json<Value>,
    ) -> axum::response::Response {
        if body["stream"] != true {
            // The compaction summary request, not a turn.
            return Json(json!({
                "id":"summary","object":"chat.completion","created":0,"model":"fixture-model",
                "choices":[{"index":0,"message":{"role":"assistant","content":"compacted summary"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}
            }))
            .into_response();
        }
        if state.main.fetch_add(1, Ordering::SeqCst) == 0 {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(json!({"error":{"type":"invalid_request_error","code":"context_length_exceeded","message":"prompt is too long"}})),
            )
                .into_response();
        }
        let stream = concat!(
            "data: {\"model\":\"fixture-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"recovered answer\"},\"finish_reason\":null}]}\n\n",
            "data: {\"model\":\"fixture-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":23,\"completion_tokens\":5}}\n\n",
            "data: [DONE]\n\n"
        );
        let _ = &state.marker;
        ([("content-type", "text/event-stream")], stream).into_response()
    }

    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    let workspace = root.path().join("project");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&workspace).unwrap();
    let marker = workspace.join("compact-hook-ran");
    let store = deepseek_code_core::runtime::SessionStore::new(
        RuntimePaths::new(data.clone())
            .unwrap()
            .open_database()
            .unwrap(),
    );
    let id = store
        .create(&workspace, "fixture-model", "fixture")
        .unwrap();
    // Reactive compaction needs earlier conversation to compact.
    store.append(&id, "user", "seed task").unwrap();
    store.append(&id, "assistant", "seed answer").unwrap();
    std::fs::write(
        data.join("settings.json"),
        json!({
            "promptSuggestionEnabled": false,
            "hooks": {"PostCompact": [{"hooks": [{
                "type": "command",
                "command": format!("touch {}; sleep 4", marker.display()),
                "shell": "sh"
            }]}]}
        })
        .to_string(),
    )
    .unwrap();

    let fixture = Fixture {
        main: Arc::default(),
        marker: marker.clone(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/chat/completions", post(respond))
        .with_state(fixture.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    std::fs::write(
        data.join("providers.json"),
        json!([{
            "id":"fixture","name":"Fixture","base_url":url,"api_key":"fixture-only",
            "api_format":"openai","enabled":true,
            "models":[{"id":"fixture-model","name":"Fixture","enabled":true,
                "context_window":128000,"max_tokens":4096,
                "supports_web_search":false,"supports_vision":false}]
        }])
        .to_string(),
    )
    .unwrap();

    let mut command = Command::new(env!("CARGO_BIN_EXE_dcl"));
    command
        .args(["--print", "--output-format", "stream-json", "--data-dir"])
        .arg(&data)
        .arg("--path")
        .arg(&workspace)
        .args([
            "--provider",
            "fixture",
            "--model",
            "fixture-model",
            "--permission-mode",
            "ask",
            "--resume",
            &id,
            "second task",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    strip_internal_env(&mut command);
    let mut child = command.spawn().unwrap();

    // Wait until the PostCompact hook is actually running.
    let deadline = Instant::now() + Duration::from_secs(25);
    while !marker.exists() {
        assert!(
            Instant::now() < deadline,
            "the PostCompact hook never ran; stderr: {}",
            String::from_utf8_lossy(
                &child
                    .wait_with_output()
                    .await
                    .map(|out| out.stderr)
                    .unwrap_or_default()
            )
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // Throughout the hook the turn must still be owned; otherwise an observer
    // could settle it as interrupted while it is alive.
    for _ in 0..8 {
        assert!(
            deepseek_code_core::services::session_messages::execution_active(&data, &id).unwrap(),
            "the turn lost execution ownership during the compaction hook"
        );
        let record = store_db(&data)
            .with_conn(|conn| turn_repo::latest_turn(conn, &id))
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            record.status,
            turn_repo::TurnStatus::Running,
            "a live turn must not be settled: {record:?}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    let output = tokio::time::timeout(Duration::from_secs(90), child.wait_with_output())
        .await
        .expect("the retried turn must finish")
        .unwrap();
    server.abort();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fixture.main.load(Ordering::SeqCst),
        2,
        "one failed attempt plus one retry"
    );
    let record = store_db(&data)
        .with_conn(|conn| turn_repo::latest_turn(conn, &id))
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(record.status, turn_repo::TurnStatus::Completed);
    assert!(
        record.elapsed_ms.unwrap_or(0) >= 4_000,
        "measured: {record:?}"
    );
}

fn store_db(data: &std::path::Path) -> Arc<DbManager> {
    RuntimePaths::new(data.to_path_buf())
        .unwrap()
        .open_database()
        .unwrap()
}

/// Opening a session to observe a turn that died must not take its queued task
/// notification: only explicit user continuation may consume it.
#[tokio::test]
async fn observing_a_dead_owner_keeps_queued_notifications_pending() {
    let mock = Mock::start(Duration::from_millis(500), 200).await;
    let project = Project::new(&mock.url);
    let id = uuid::Uuid::new_v4().to_string();
    let mut child = owner(&project, &id, false).spawn().unwrap();
    assert!(
        wait_until(&project, &id, |progress| progress.2 > 0
            && !progress.1.is_empty())
        .await,
        "the live turn must publish progress before it is killed"
    );
    child.start_kill().unwrap();
    let _ = child.wait().await;

    // A finished background task left a notification for this session.
    deepseek_code_core::services::input_queue::push(
        &project.db,
        &id,
        "<task-notification>background build finished</task-notification>",
    )
    .unwrap();
    let pending = |project: &Project, id: &str| {
        deepseek_code_core::services::input_queue::any(&project.db, id, |_| true).unwrap()
    };
    assert!(pending(&project, &id));

    let requests_before = mock.count();
    let output = resume_without_a_turn(&project, &id).await;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        mock.count(),
        requests_before,
        "observation requested the model"
    );
    assert!(
        pending(&project, &id),
        "observation consumed the queued notification"
    );
    let settled = project
        .db
        .with_conn(|conn| turn_repo::latest_turn(conn, &id))
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(settled.interrupted());
}
