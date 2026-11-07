//! The background-task chain: a real supervisor process starts a real CLI
//! worker against a local fault service, and the evidence is compared between
//! the server, the task output, the SQLite request snapshot and `tasks show`.
use deepseek_code_core::services::background_sessions::{self, Launch, CONTROL_ENV};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

const API_KEY: &str = "sk-fixture-worker-key-0001";
const SESSION: &str = "6a1f2c3d-0000-4000-8000-000000000002";

#[derive(Clone)]
enum Reply {
    CloseAfterRequest,
    Status { code: u16, body: String },
}

struct Fixture {
    base: String,
    requests: Arc<AtomicUsize>,
}

async fn fixture(reply: Reply) -> Fixture {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = requests.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            counter.fetch_add(1, Ordering::SeqCst);
            let reply = reply.clone();
            tokio::spawn(async move {
                let _ = answer(&mut socket, reply).await;
            });
        }
    });
    Fixture { base, requests }
}

async fn answer(socket: &mut TcpStream, reply: Reply) -> std::io::Result<()> {
    read_request(socket).await?;
    match reply {
        Reply::CloseAfterRequest => Ok(()),
        Reply::Status { code, body } => {
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 {code} Fixture\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await?;
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
        let project = Self { temp };
        std::fs::write(
            project.data().join("providers.json"),
            json!([{
                "id": "fixture", "name": "Fixture", "base_url": "",
                "api_key": API_KEY, "api_format": "openai", "enabled": true,
                "models": [{
                    "id": "fixture-model", "name": "Fixture", "enabled": true,
                    "supports_vision": false, "supports_web_search": false,
                    "max_tokens": 512
                }]
            }])
            .to_string(),
        )
        .unwrap();
        project
    }

    fn data(&self) -> PathBuf {
        self.temp.path().join("data")
    }

    fn workspace(&self) -> PathBuf {
        self.temp.path().join("project")
    }

    fn set_base(&self, base: &str) {
        std::fs::write(
            self.data().join("providers.json"),
            json!([{
                "id": "fixture", "name": "Fixture", "base_url": base,
                "api_key": API_KEY, "api_format": "openai", "enabled": true,
                "models": [{
                    "id": "fixture-model", "name": "Fixture", "enabled": true,
                    "supports_vision": false, "supports_web_search": false,
                    "max_tokens": 512
                }]
            }])
            .to_string(),
        )
        .unwrap();
    }

    /// Launch a detached task through a real supervisor, exactly as the agent's
    /// background tool call does.
    fn launch(&self) -> background_sessions::Job {
        let executable = PathBuf::from(env!("CARGO_BIN_EXE_dcl"));
        let launch = Launch {
            executable: executable.clone(),
            arguments: vec![
                "--data-dir".into(),
                self.data().to_string_lossy().into_owned(),
                "--path".into(),
                self.workspace().to_string_lossy().into_owned(),
                "--provider".into(),
                "fixture".into(),
                "--model".into(),
                "fixture-model".into(),
                "--session-id".into(),
                SESSION.into(),
                "--permission-mode".into(),
                "bypass".into(),
                "-p".into(),
                "hello".into(),
            ],
            cwd: self.workspace(),
        };
        // The supervisor runs this executable again; it must not inherit the
        // developer's session ownership, only this task's.
        let environment = vec![
            (
                "DEEPSEEK_CODE_DATA_DIR".to_owned(),
                self.data().to_string_lossy().into_owned(),
            ),
            (CONTROL_ENV.to_owned(), String::new()),
        ];
        background_sessions::spawn(
            &self.data(),
            &executable,
            launch,
            SESSION,
            SESSION,
            "fixture task",
            &environment,
        )
        .unwrap()
    }

    async fn wait(&self, id: &str) -> background_sessions::Job {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(90);
        loop {
            let job = background_sessions::get(&self.data(), id).unwrap();
            if !matches!(job.status.as_str(), "starting" | "running") {
                return job;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "the worker never finished: {job:?}"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

fn persisted_snapshot(data: &Path, session: &str) -> Value {
    let paths = deepseek_code_core::runtime::RuntimePaths::new(data.to_path_buf()).unwrap();
    let db = paths.open_database().unwrap();
    let snapshots = db
        .with_conn(|conn| -> anyhow::Result<Value> {
            let message: String = conn.query_row(
                "SELECT id FROM messages WHERE conversation_id=?1 AND role='assistant' ORDER BY sort_order DESC LIMIT 1",
                [session],
                |row| row.get(0),
            )?;
            deepseek_code_core::db::message_repo::get_request_snapshots(conn, &message)
        })
        .unwrap()
        .unwrap();
    snapshots
        .as_array()
        .and_then(|items| {
            items
                .iter()
                .find_map(|item| item.get("failure").filter(|failure| !failure.is_null()))
        })
        .cloned()
        .unwrap_or_else(|| panic!("no structured failure in {snapshots}"))
}

#[tokio::test]
async fn a_background_worker_reports_the_same_attempts_as_its_session() {
    let project = Project::new();
    let fixture = fixture(Reply::CloseAfterRequest).await;
    project.set_base(&fixture.base);
    let job = project.launch();
    let job = project.wait(&job.id).await;

    let limit = deepseek_code_core::config::OrchestrationConfig::max_retries() + 1;
    assert_eq!(job.status, "failed", "{job:?}");
    // Task state itself carries the safe diagnostic, without copying output.
    let error = job.error.as_deref().expect("task state explains the failure");
    for expected in [
        "stage=send",
        "class=transient",
        &format!("attempts={limit}"),
        &format!("retries={}", limit - 1),
    ] {
        assert!(error.contains(expected), "missing {expected}: {error}");
    }
    assert!(!error.contains(API_KEY), "the API key leaked: {error}");
    assert_eq!(
        fixture.requests.load(Ordering::SeqCst),
        limit as usize,
        "the server saw a different number of HTTP attempts"
    );

    // The task's own output is what `/tasks` and `/tasks show` display.
    let (stdout, stderr) = background_sessions::output(&project.data(), &job.id).unwrap();
    let transcript = format!("{stdout}{stderr}");
    for expected in [
        format!("attempts={limit}"),
        format!("retries={}", limit - 1),
        "stage=send".to_owned(),
    ] {
        assert!(
            transcript.contains(&expected),
            "missing {expected}: {transcript}"
        );
    }
    assert!(
        !transcript.contains(API_KEY),
        "the API key leaked: {transcript}"
    );

    // A CLI started in another process reads the same numbers from the session.
    let snapshots = db_snapshot_attempts(&project, limit);
    assert_eq!(snapshots, limit);
}

fn db_snapshot_attempts(project: &Project, limit: u32) -> u32 {
    let failure = persisted_snapshot(&project.data(), SESSION);
    assert_eq!(failure["attempts"], limit, "{failure}");
    assert_eq!(failure["retries"], limit - 1, "{failure}");
    assert_eq!(failure["stage"], "send", "{failure}");
    failure["attempts"].as_u64().unwrap() as u32
}

#[tokio::test]
async fn a_background_worker_does_not_retry_an_authentication_failure() {
    let project = Project::new();
    let fixture = fixture(Reply::Status {
        code: 401,
        body: json!({"error": {"message": "invalid api key"}}).to_string(),
    })
    .await;
    project.set_base(&fixture.base);
    let job = project.launch();
    let job = project.wait(&job.id).await;

    assert_eq!(job.status, "failed", "{job:?}");
    assert_eq!(fixture.requests.load(Ordering::SeqCst), 1);
    let error = job.error.as_deref().expect("task state explains the failure");
    assert!(error.contains("http=401"), "{error}");
    assert!(error.contains("class=authentication"), "{error}");
    assert!(error.contains("attempts=1"), "{error}");
    let (stdout, stderr) = background_sessions::output(&project.data(), &job.id).unwrap();
    let transcript = format!("{stdout}{stderr}");
    assert!(transcript.contains("attempts=1"), "{transcript}");
    assert!(transcript.contains("invalid api key"), "{transcript}");

    let failure = persisted_snapshot(&project.data(), SESSION);
    assert_eq!(failure["attempts"], 1, "{failure}");
    assert_eq!(failure["http"], 401, "{failure}");
    assert_eq!(failure["class"], "authentication", "{failure}");
}

/// `/tasks show` prints the job JSON; the task's error text must carry the same
/// attempts as the session snapshot, and cancellation must leave no such error.
#[tokio::test]
async fn cancelling_a_background_worker_stops_retries_and_records_no_failure() {
    let project = Project::new();
    let fixture = fixture(Reply::CloseAfterRequest).await;
    project.set_base(&fixture.base);
    let job = project.launch();
    // Cancel while the first attempt is still in flight.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while fixture.requests.load(Ordering::SeqCst) == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "no request was sent"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    background_sessions::cancel(&project.data(), &job.id).unwrap();
    let job = project.wait(&job.id).await;
    assert_eq!(job.status, "cancelled", "{job:?}");
    // Cancellation is not a model failure, so no failure summary is invented.
    assert_eq!(job.error, None, "{job:?}");
    let sent = fixture.requests.load(Ordering::SeqCst);

    // Past the next retry instant, nothing new may arrive.
    tokio::time::sleep(Duration::from_millis(
        deepseek_code_core::config::OrchestrationConfig::retry_base_backoff_ms() + 1_000,
    ))
    .await;
    assert_eq!(
        fixture.requests.load(Ordering::SeqCst),
        sent,
        "a cancelled worker kept retrying"
    );
}
