// A consumer that goes away must cancel the recovery wait, the replacement
// open and the replacement read. These tests drive the real executor against a
// loopback fixture so the closure is observed at each of those three points.
use deepseek_code_core::native_engine::{
    engine_core::ReasoningMode,
    provider_manager::ResolvedProvider,
    tool_loop::{EngineEvent, ToolLoopExecutor},
};
use serde_json::json;
use std::{
    io,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::Notify,
};

const SSE_HEAD: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n";

const FIRST_TEXT: &str =
    "data: {\"choices\":[{\"delta\":{\"content\":\"stalled text\"},\"finish_reason\":null}]}\n\n";
const REPLACE_TEXT: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"replacement text\"},\"finish_reason\":null}]}\n\n";
const REPLACE_THINKING: &str = "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"replacement thinking\"},\"finish_reason\":null}]}\n\n";
const REPLACE_TOOL: &str = "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-orphan\",\"type\":\"function\",\"function\":{\"name\":\"Write\",\"arguments\":\"{\\\"file_path\\\":\\\"orphan.txt\\\"\"}}]},\"finish_reason\":null}]}\n\n";
const REPLACE_USAGE: &str =
    "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":1}}\n\n";
const COMPLETE: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"unauthorized\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

struct Fixture {
    base: String,
    posts: Arc<AtomicUsize>,
    /// The replacement request head was read and its headers withheld.
    replacement_read: Arc<Notify>,
}

fn provider(base: &str) -> ResolvedProvider {
    ResolvedProvider {
        provider: serde_json::from_value(json!({
            "id": "fixture", "name": "Fixture", "base_url": base,
            "api_key": "fixture-key", "api_format": "openai",
            "models": [], "enabled": true
        }))
        .unwrap(),
        model: serde_json::from_value(json!({
            "id": "fixture-model", "name": "Fixture", "enabled": true,
            "max_tokens": 256, "context_window": 128000,
            "supports_vision": false, "supports_web_search": false
        }))
        .unwrap(),
    }
}

/// One scripted POST per phase; every write is flushed before it signals.
async fn start_fixture(branch: &'static str) -> Fixture {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let posts = Arc::new(AtomicUsize::new(0));
    let replacement_read = Arc::new(Notify::new());
    let (counts, read_signal) = (posts.clone(), replacement_read.clone());
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let (counts, read_signal) = (counts.clone(), read_signal.clone());
            tokio::spawn(async move {
                if read_head(&mut socket).await.is_err() {
                    return;
                }
                let post = counts.fetch_add(1, Ordering::SeqCst) + 1;
                let first = post == 1;
                let second = post == 2;
                match (branch, first, second) {
                    (_, true, _) => {
                        let _ = write_body(&mut socket, &[FIRST_TEXT], false).await;
                        hold().await;
                    }
                    ("header", _, true) => {
                        // Complete POST, headers withheld.
                        read_signal.notify_one();
                        hold().await;
                    }
                    ("body", _, true) => {
                        let _ = write_body(
                            &mut socket,
                            &[REPLACE_TEXT, REPLACE_THINKING, REPLACE_TOOL, REPLACE_USAGE],
                            false,
                        )
                        .await;
                        hold().await;
                    }
                    _ => {
                        let _ = write_body(&mut socket, &[COMPLETE], true).await;
                        let _ = socket.shutdown().await;
                    }
                }
            });
        }
    });
    Fixture {
        base,
        posts,
        replacement_read,
    }
}

async fn hold() {
    tokio::time::sleep(Duration::from_secs(600)).await;
}

async fn read_head(socket: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = socket.read(&mut chunk).await?;
        if read == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "closed"));
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    Ok(buffer)
}

async fn write_body(socket: &mut TcpStream, parts: &[&str], end: bool) -> io::Result<()> {
    socket.write_all(SSE_HEAD).await?;
    for part in parts {
        socket
            .write_all(format!("{:x}\r\n{part}\r\n", part.len()).as_bytes())
            .await?;
        socket.flush().await?;
    }
    if end {
        socket.write_all(b"0\r\n\r\n").await?;
    }
    socket.flush().await
}

fn config(idle: u64, backoff: u64) -> String {
    format!(
        "[tool_loop]\n[tools]\n[memory]\n[bridge]\n[http]\n\
         request_timeout_secs = {idle}\nmax_retries = 1\nretry_base_backoff_ms = {backoff}\n\
         post_content_timeout_recovery = true\n[logging]\n[telemetry]\n"
    )
}

struct Plan {
    idle: u64,
    backoff: u64,
    watchdog: Duration,
    posts: usize,
}

/// The backoff watchdog is well below the configured wait; the open and read
/// watchdogs are well below the configured idle deadline.
fn plan(branch: &str) -> Plan {
    match branch {
        "backoff" => Plan {
            idle: 1,
            backoff: 5000,
            watchdog: Duration::from_millis(2000),
            posts: 1,
        },
        _ => Plan {
            idle: 3,
            backoff: 20,
            watchdog: Duration::from_millis(1000),
            posts: 2,
        },
    }
}

#[tokio::test]
async fn post_content_recovery_receiver_close_cancels_backoff_open_and_read() {
    const CHILD: &str = "DCL_RECOVERY_CLOSE_CHILD";
    let Ok(branch) = std::env::var(CHILD) else {
        // Each branch needs its own process: the orchestration singleton is
        // loaded once, from the child's cwd-local file.
        for branch in ["backoff", "header", "body"] {
            let temp = tempfile::tempdir().unwrap();
            std::fs::create_dir_all(temp.path().join("config")).unwrap();
            let settings = plan(branch);
            deepseek_code_core::data_layout::write_config_file(
                temp.path().join("config/orchestration.toml"),
                config(settings.idle, settings.backoff),
            )
            .unwrap();
            let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "post_content_recovery_receiver_close_cancels_backoff_open_and_read",
                    "--nocapture",
                ])
                .current_dir(temp.path())
                .env(CHILD, branch)
                .env_remove("DEEPSEEK_CODE_DATA_DIR")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let mut stdout = child.stdout.take().unwrap();
            let mut stderr = child.stderr.take().unwrap();
            let out = tokio::spawn(async move {
                let mut text = String::new();
                let _ = stdout.read_to_string(&mut text).await;
                text
            });
            let err = tokio::spawn(async move {
                let mut text = String::new();
                let _ = stderr.read_to_string(&mut text).await;
                text
            });
            let status = match tokio::time::timeout(Duration::from_secs(90), child.wait()).await {
                Ok(status) => status.unwrap(),
                Err(_) => {
                    let _ = child.kill().await;
                    panic!("{branch}: the child did not finish");
                }
            };
            let output = format!("{}{}", out.await.unwrap(), err.await.unwrap());
            assert!(
                output.contains("1 passed") && status.success(),
                "{branch}: the child did not pass: {output}"
            );
        }
        return;
    };
    let branch: &'static str = match branch.as_str() {
        "header" => "header",
        "body" => "body",
        _ => "backoff",
    };
    let settings = plan(branch);
    let fixture = start_fixture(branch).await;
    let (tx, mut rx) = tokio::sync::mpsc::channel(256);
    let cwd = std::env::current_dir().unwrap();
    let mut executor = ToolLoopExecutor::new(
        provider(&fixture.base),
        vec![json!({"role": "user", "content": "stall please"})],
        None,
        256,
        tx,
        cwd.to_string_lossy().into_owned(),
        ReasoningMode::Standard,
    );
    let mut exec = Box::pin(executor.execute());

    // Wait for the phase that proves the executor reached the point under test.
    loop {
        match branch {
            "backoff" => {
                tokio::select! {
                    result = &mut exec => panic!("{branch}: the loop ended early: {result:?}"),
                    event = rx.recv() => {
                        let Some(event) = event else {
                            panic!("{branch}: the events closed before closure");
                        };
                        if let EngineEvent::RequestAttempt(event) = event {
                            if event["outcome"] == "post_content_retry" {
                                break;
                            }
                        }
                    }
                }
            }
            "header" => {
                tokio::select! {
                    result = &mut exec => panic!("{branch}: the loop ended early: {result:?}"),
                    _ = fixture.replacement_read.notified() => break,
                    event = rx.recv() => {
                        assert!(event.is_some(), "{branch}: the events closed early");
                    }
                }
            }
            _ => {
                tokio::select! {
                    result = &mut exec => panic!("{branch}: the loop ended early: {result:?}"),
                    event = rx.recv() => {
                        let Some(event) = event else {
                            panic!("{branch}: the events closed early");
                        };
                        if let EngineEvent::RequestSnapshot(snapshot) = event {
                            // Usage proves the replacement body was consumed.
                            if snapshot["usage"]
                                .as_object()
                                .is_some_and(|usage| !usage.is_empty())
                            {
                                break;
                            }
                        }
                    }
                }
            }
        }
    }

    let dropped_at = Instant::now();
    drop(rx);
    let outcome = tokio::time::timeout(settings.watchdog, &mut exec)
        .await
        .unwrap_or_else(|_| panic!("{branch}: a closed consumer did not cancel the executor"));
    let elapsed = dropped_at.elapsed();
    let (text, stop) = outcome.expect("the loop reports Ok when the consumer closes");
    assert_eq!(stop.as_deref(), Some("interrupted"), "{branch}");
    assert_eq!(text, "", "{branch}: uncommitted text was returned");
    assert!(
        elapsed < settings.watchdog,
        "{branch}: closure took {elapsed:?}, so the pending timer won"
    );
    assert_eq!(
        fixture.posts.load(Ordering::SeqCst),
        settings.posts,
        "{branch}: an unauthorized POST was issued"
    );
    assert!(
        !std::path::Path::new("orphan.txt").exists(),
        "{branch}: a partial tool call was dispatched"
    );
}
