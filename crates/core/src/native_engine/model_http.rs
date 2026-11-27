//! Model streaming transport with process-wide shared HTTP connection pools.
//!
//! A stream is bounded by an idle read deadline rather than a wall-clock
//! deadline for the whole exchange. Long answers and slow providers therefore
//! stay connected as long as bytes keep arriving, while a stalled header wait or
//! a silent body still fails within `[http].request_timeout_secs`.
//!
//! All production streaming model requests share three process-wide
//! `reqwest::Client` pools separated by transport semantics:
//! - `Redirect::Default` (direct OpenAI-compatible chat streams),
//! - `Redirect::None` (built-in providers, Anthropic, `provider_runtime`, `followup`),
//! - `single_post` (replacement POSTs / suggestion guard: no redirects and no
//!   reqwest internal retries).
//!
//! Do not build a fresh `reqwest::Client` per request on streaming paths: doing
//! so discards the HTTP/1.1 keep-alive pool and forces a new TCP/TLS handshake
//! on every call. `[http].request_timeout_secs`, `[http].pool_idle_timeout_secs`,
//! and proxy environment variables are read when each shared client is first
//! built; changes take effect after restarting the process.
use anyhow::{bail, Context, Result};
use std::sync::OnceLock;
use std::time::Duration;

const POOL_MAX_IDLE_PER_HOST: usize = 32;

struct SharedClients {
    follow_redirects: OnceLock<std::result::Result<reqwest::Client, String>>,
    no_redirects: OnceLock<std::result::Result<reqwest::Client, String>>,
    single_post: OnceLock<std::result::Result<reqwest::Client, String>>,
}

static SHARED: SharedClients = SharedClients {
    follow_redirects: OnceLock::new(),
    no_redirects: OnceLock::new(),
    single_post: OnceLock::new(),
};

/// Redirect behaviour the transport already used before this module existed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Redirect {
    /// reqwest's default bounded redirect handling.
    Default,
    /// Never follow a redirect.
    None,
}

impl Redirect {
    fn policy(self) -> reqwest::redirect::Policy {
        match self {
            Self::Default => reqwest::redirect::Policy::default(),
            Self::None => reqwest::redirect::Policy::none(),
        }
    }
}

/// The configured idle deadline.
///
/// The fallible initialization runs before the getter: the getter caches built-in
/// defaults after a parse error, so a check after it could never see the
/// difference for a standalone caller.
pub(super) fn configured_idle() -> Result<Duration> {
    crate::config::OrchestrationConfig::init_global()?;
    idle_seconds(crate::config::OrchestrationConfig::request_timeout_secs())
}

fn configured_pool_idle() -> Result<Duration> {
    crate::config::OrchestrationConfig::init_global()?;
    pool_idle_seconds(crate::config::OrchestrationConfig::pool_idle_timeout_secs())
}

pub(super) fn idle_seconds(seconds: u64) -> Result<Duration> {
    if seconds == 0 {
        bail!(
            "[http].request_timeout_secs must be positive; zero would abort every model stream as soon as it is idle"
        );
    }
    Ok(Duration::from_secs(seconds))
}

fn pool_idle_seconds(seconds: u64) -> Result<Duration> {
    if seconds == 0 {
        bail!(
            "[http].pool_idle_timeout_secs must be positive; zero would close every idle model connection immediately"
        );
    }
    Ok(Duration::from_secs(seconds))
}

fn build_streaming_client(
    idle: Duration,
    pool_idle: Duration,
    redirect: Redirect,
) -> Result<reqwest::Client> {
    if idle.is_zero() {
        bail!("model stream idle timeout must be positive");
    }
    if pool_idle.is_zero() {
        bail!("model pool idle timeout must be positive");
    }
    reqwest::Client::builder()
        .read_timeout(idle)
        // No `.timeout(...)`: the exchange is bounded per read, not in total.
        .pool_idle_timeout(pool_idle)
        .pool_max_idle_per_host(POOL_MAX_IDLE_PER_HOST)
        .redirect(redirect.policy())
        .build()
        .context("building the model streaming client")
}

fn build_single_post_client(idle: Duration, pool_idle: Duration) -> Result<reqwest::Client> {
    if idle.is_zero() {
        bail!("model stream idle timeout must be positive");
    }
    if pool_idle.is_zero() {
        bail!("model pool idle timeout must be positive");
    }
    reqwest::Client::builder()
        .read_timeout(idle)
        .pool_idle_timeout(pool_idle)
        .pool_max_idle_per_host(POOL_MAX_IDLE_PER_HOST)
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .build()
        .context("building the single-attempt model streaming client")
}

/// Shared model-stream client: no total deadline, one idle deadline per read,
/// backed by a process-wide keep-alive pool keyed by redirect policy.
pub(super) fn streaming_client(redirect: Redirect) -> Result<reqwest::Client> {
    let slot = match redirect {
        Redirect::Default => &SHARED.follow_redirects,
        Redirect::None => &SHARED.no_redirects,
    };
    slot.get_or_init(|| {
        let idle = configured_idle().map_err(|error| format!("{error:#}"))?;
        let pool = configured_pool_idle().map_err(|error| format!("{error:#}"))?;
        build_streaming_client(idle, pool, redirect).map_err(|error| format!("{error:#}"))
    })
    .clone()
    .map_err(anyhow::Error::msg)
}

/// Test helper returning an isolated streaming client with an explicit idle
/// read timeout; does not read or populate the global shared pools.
#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn streaming_client_with_idle(
    idle: Duration,
    redirect: Redirect,
) -> Result<reqwest::Client> {
    build_streaming_client(idle, Duration::from_secs(90), redirect)
}

/// Shared client for one authorized replacement POST.
///
/// Redirects and reqwest's internal protocol-nack retries are disabled, so an
/// application budget of one really means one physical request. Direct OpenAI
/// ordinarily follows redirects, and the default client may retry a protocol
/// nack on its own, either of which would spend an unauthorized second POST.
/// Kept in its own pool slot so single-post semantics stay isolated from normal
/// streaming requests.
pub(super) fn single_post_client() -> Result<reqwest::Client> {
    SHARED
        .single_post
        .get_or_init(|| {
            let idle = configured_idle().map_err(|error| format!("{error:#}"))?;
            let pool = configured_pool_idle().map_err(|error| format!("{error:#}"))?;
            build_single_post_client(idle, pool).map_err(|error| format!("{error:#}"))
        })
        .clone()
        .map_err(anyhow::Error::msg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    type Handler = Arc<dyn Fn(TcpStream) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

    /// One loopback server per test; every accepted socket gets its own script.
    async fn server(handler: Handler) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let connections = Arc::new(AtomicUsize::new(0));
        let counted = connections.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                counted.fetch_add(1, Ordering::SeqCst);
                let handler = handler.clone();
                tokio::spawn(async move { handler(socket).await });
            }
        });
        (base, connections, task)
    }

    fn handler<F, Fut>(f: F) -> Handler
    where
        F: Fn(TcpStream) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        Arc::new(move |socket| Box::pin(f(socket)))
    }

    async fn read_request(socket: &mut TcpStream) -> bool {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let read = socket.read(&mut chunk).await.unwrap_or(0);
            if read == 0 {
                return false;
            }
            buffer.extend_from_slice(&chunk[..read]);
            let Some(end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") else {
                continue;
            };
            let head = String::from_utf8_lossy(&buffer[..end]).to_ascii_lowercase();
            let length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            while buffer.len() < end + 4 + length {
                let read = socket.read(&mut chunk).await.unwrap_or(0);
                if read == 0 {
                    return false;
                }
                buffer.extend_from_slice(&chunk[..read]);
            }
            return true;
        }
    }

    async fn keep_alive_server(
        responses: usize,
    ) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let accepts = Arc::new(AtomicUsize::new(0));
        let counted = accepts.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                counted.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    for _ in 0..responses {
                        if !read_request(&mut socket).await {
                            return;
                        }
                        if socket
                            .write_all(
                                b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\ncontent-type: text/plain\r\n\r\nok",
                            )
                            .await
                            .is_err()
                        {
                            return;
                        }
                        if socket.flush().await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        (base, accepts, task)
    }

    async fn write_chunk(socket: &mut TcpStream, bytes: &[u8]) {
        let mut frame = format!("{:x}\r\n", bytes.len()).into_bytes();
        frame.extend_from_slice(bytes);
        frame.extend_from_slice(b"\r\n");
        socket.write_all(&frame).await.unwrap();
        socket.flush().await.unwrap();
    }

    const SSE_HEADERS: &[u8] = b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n";

    #[tokio::test]
    async fn stream_read_timeout_resets_on_each_wire_chunk() {
        let idle = Duration::from_millis(600);
        let frame = "data: {\"choices\":[{\"delta\":{\"content\":\"中文 🦀\"},\"finish_reason\":null}]}\n\n";
        // Split inside the three-byte scalar, so only a decoder that waits for
        // the rest of the code point can produce the expected text.
        let cut = frame.find('中').unwrap() + 1;
        let mut parts: Vec<Vec<u8>> = vec![b": keepalive\n\n".repeat(4)];
        parts.push(frame.as_bytes()[..cut].to_vec());
        parts.push(frame.as_bytes()[cut..].to_vec());
        parts.push(b"data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_vec());
        parts.push(b"data: [DONE]\n\n".to_vec());
        parts.extend(std::iter::repeat(b": keepalive\n\n".to_vec()).take(4));
        let expected = String::from_utf8(parts.concat()).unwrap();

        let script = parts.clone();
        let (base, connections, task) = server(handler(move |mut socket| {
            let script = script.clone();
            async move {
                read_request(&mut socket).await;
                socket.write_all(SSE_HEADERS).await.unwrap();
                socket.flush().await.unwrap();
                for part in script {
                    write_chunk(&mut socket, &part).await;
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
                socket.write_all(b"0\r\n\r\n").await.unwrap();
                socket.flush().await.unwrap();
            }
        }))
        .await;
        let client = streaming_client_with_idle(idle, Redirect::None).unwrap();
        let started = std::time::Instant::now();
        let response = client.post(&base).body("{}").send().await.unwrap();
        let mut stream = Box::pin(super::super::utf8_stream::response(response));
        let mut decoded = String::new();
        while let Some(chunk) = stream.next().await {
            decoded.push_str(&chunk.unwrap());
        }
        let elapsed = started.elapsed();
        task.abort();
        assert_eq!(
            decoded, expected,
            "the decoded stream lost or reordered bytes"
        );
        let done = decoded
            .find("data: [DONE]\n\n")
            .expect("the completion marker arrived");
        assert!(decoded[..done].contains("中文 🦀"), "{decoded}");
        assert_eq!(decoded.matches("[DONE]").count(), 1, "{decoded}");
        assert!(
            elapsed > idle * 2,
            "the response must outlive two idle windows: {elapsed:?}"
        );
        assert_eq!(connections.load(Ordering::SeqCst), 1, "one model POST");
    }

    fn hold_open() -> impl Fn(TcpStream) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send>> {
        |mut socket| {
            Box::pin(async move {
                read_request(&mut socket).await;
                // Keep the socket open until the client gives up and closes it.
                let mut buffer = [0u8; 256];
                while let Ok(read) = socket.read(&mut buffer).await {
                    if read == 0 {
                        return;
                    }
                }
            })
        }
    }

    #[tokio::test]
    async fn stream_read_timeout_distinguishes_stalled_headers_and_bodies() {
        let idle = Duration::from_millis(400);
        assert!(
            streaming_client_with_idle(Duration::ZERO, Redirect::None).is_err(),
            "zero must be rejected before a client exists"
        );
        let client = streaming_client_with_idle(idle, Redirect::None).unwrap();

        // 1. No headers at all: the deadline hits the header wait, i.e. send.
        let (base, connections, task) = server(handler(hold_open())).await;
        let error = client.post(&base).body("{}").send().await.unwrap_err();
        assert!(error.is_timeout(), "header wait must be a timeout: {error}");
        assert!(
            !error.is_body(),
            "the header wait is not a body failure: {error}"
        );
        assert_eq!(connections.load(Ordering::SeqCst), 1);
        task.abort();

        // 2. Flushed headers with zero body bytes: the body reader fails.
        let (base, connections, task) = server(handler(|mut socket| async move {
            read_request(&mut socket).await;
            socket.write_all(SSE_HEADERS).await.unwrap();
            socket.flush().await.unwrap();
            tokio::time::sleep(Duration::from_secs(30)).await;
        }))
        .await;
        let response = client.post(&base).body("{}").send().await.unwrap();
        assert_eq!(response.status().as_u16(), 200);
        let mut body = response.bytes_stream();
        let error = body.next().await.expect("a body item").unwrap_err();
        assert!(
            error.is_timeout() && (error.is_body() || error.is_decode()),
            "a stalled body must be a body timeout: {error}"
        );
        assert_eq!(connections.load(Ordering::SeqCst), 1);
        task.abort();

        // 3. Delivered bytes, then silence: the next read times out, and a
        // dropped pending read closes the body instead.
        for drop_early in [false, true] {
            let peer_closed = Arc::new(AtomicBool::new(false));
            let flag = peer_closed.clone();
            let (base, connections, task) = server(handler(move |mut socket| {
                let flag = flag.clone();
                async move {
                    read_request(&mut socket).await;
                    socket.write_all(SSE_HEADERS).await.unwrap();
                    write_chunk(&mut socket, b"data: partial\n\n").await;
                    let mut buffer = [0u8; 256];
                    while let Ok(read) = socket.read(&mut buffer).await {
                        if read == 0 {
                            flag.store(true, Ordering::SeqCst);
                            return;
                        }
                    }
                }
            }))
            .await;
            let response = client.post(&base).body("{}").send().await.unwrap();
            let mut body = response.bytes_stream();
            let first = body.next().await.expect("a first chunk").unwrap();
            assert!(String::from_utf8_lossy(&first).contains("partial"));
            if drop_early {
                drop(body);
                for _ in 0..200 {
                    if peer_closed.load(Ordering::SeqCst) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                assert!(
                    peer_closed.load(Ordering::SeqCst),
                    "dropping the read must close the body"
                );
            } else {
                let error = body.next().await.expect("a second item").unwrap_err();
                assert!(
                    error.is_timeout() && (error.is_body() || error.is_decode()),
                    "silence after bytes must be a body timeout: {error}"
                );
            }
            assert_eq!(connections.load(Ordering::SeqCst), 1);
            task.abort();
        }
    }

    /// A configured zero is a setup error; it must never reach a model POST.
    #[tokio::test]
    async fn configured_zero_idle_fails_before_any_model_post() {
        const CHILD: &str = "DCL_MODEL_HTTP_ZERO_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let error = streaming_client(Redirect::None).unwrap_err();
            let text = format!("{error:#}");
            assert!(text.contains("request_timeout_secs"), "{text}");
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("config")).unwrap();
        crate::data_layout::write_config_file(
            temp.path().join("config/orchestration.toml"),
            "[tool_loop]\n[tools]\n[memory]\n[bridge]\n[http]\nrequest_timeout_secs = 0\n\
             [logging]\n[telemetry]\n",
        )
        .unwrap();
        let (base, connections, task) = server(handler(hold_open())).await;
        let _ = base;
        let child = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "native_engine::model_http::tests::configured_zero_idle_fails_before_any_model_post",
                "--nocapture",
            ])
            .current_dir(temp.path())
            .env(CHILD, "1")
            .env_remove("DEEPSEEK_CODE_DATA_DIR")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await
            .unwrap();
        let output = format!(
            "{}{}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
        assert!(output.contains("1 passed"), "{output}");
        assert!(child.status.success(), "{output}");
        task.abort();
        assert_eq!(
            connections.load(Ordering::SeqCst),
            0,
            "the invalid configuration opened a model POST"
        );
    }

    #[tokio::test]
    async fn two_stream_requests_reuse_one_connection() {
        for redirect in [Redirect::None, Redirect::Default] {
            let (base, accepts, task) = keep_alive_server(2).await;
            let first = streaming_client(redirect).unwrap();
            let body1 = first
                .post(&base)
                .body("{}")
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
            assert_eq!(body1, "ok");

            // Calling `streaming_client` again must return a handle to the same
            // process-wide connection pool and reuse the idle TCP socket.
            let second = streaming_client(redirect).unwrap();
            let body2 = second
                .post(&base)
                .body("{}")
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
            assert_eq!(body2, "ok");
            assert_eq!(
                accepts.load(Ordering::SeqCst),
                1,
                "{redirect:?}: two sequential streaming requests must reuse one TCP connection"
            );
            task.abort();
        }
    }

    #[tokio::test]
    async fn redirect_policies_stay_separate() {
        let (target_base, target_accepts, target_task) = server(handler(|mut socket| async move {
            read_request(&mut socket).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                )
                .await
                .unwrap();
            socket.flush().await.unwrap();
        }))
        .await;

        let location = target_base.clone();
        let (source_base, source_accepts, source_task) = server(handler(move |mut socket| {
            let location = location.clone();
            async move {
                read_request(&mut socket).await;
                let response = format!(
                    "HTTP/1.1 302 Found\r\nlocation: {location}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                );
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.flush().await.unwrap();
            }
        }))
        .await;

        let followed = streaming_client(Redirect::Default)
            .unwrap()
            .post(&source_base)
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(followed.status().as_u16(), 200);
        assert_eq!(followed.text().await.unwrap(), "ok");
        assert_eq!(source_accepts.load(Ordering::SeqCst), 1);
        assert_eq!(target_accepts.load(Ordering::SeqCst), 1);

        let not_followed = streaming_client(Redirect::None)
            .unwrap()
            .post(&source_base)
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(not_followed.status().as_u16(), 302);
        assert_eq!(source_accepts.load(Ordering::SeqCst), 2);
        assert_eq!(
            target_accepts.load(Ordering::SeqCst),
            1,
            "Redirect::None must not follow redirects to the target server"
        );

        source_task.abort();
        target_task.abort();
    }

    #[tokio::test]
    async fn single_post_never_follows_or_retries() {
        let (target_base, target_accepts, target_task) = server(handler(|mut socket| async move {
            read_request(&mut socket).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                )
                .await
                .unwrap();
            socket.flush().await.unwrap();
        }))
        .await;

        let location = target_base.clone();
        let (source_base, source_accepts, source_task) = server(handler(move |mut socket| {
            let location = location.clone();
            async move {
                read_request(&mut socket).await;
                let response = format!(
                    "HTTP/1.1 307 Temporary Redirect\r\nlocation: {location}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                );
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.flush().await.unwrap();
            }
        }))
        .await;

        let response = single_post_client()
            .unwrap()
            .post(&source_base)
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 307);
        assert_eq!(source_accepts.load(Ordering::SeqCst), 1);
        assert_eq!(
            target_accepts.load(Ordering::SeqCst),
            0,
            "single_post_client must not follow redirects"
        );

        source_task.abort();
        target_task.abort();
    }

    #[tokio::test]
    async fn pool_idle_timeout_must_be_positive() {
        const CHILD: &str = "DCL_MODEL_HTTP_ZERO_POOL_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let error = streaming_client(Redirect::None).unwrap_err();
            let text = format!("{error:#}");
            assert!(text.contains("pool_idle_timeout_secs"), "{text}");
            let single_error = single_post_client().unwrap_err();
            let single_text = format!("{single_error:#}");
            assert!(
                single_text.contains("pool_idle_timeout_secs"),
                "{single_text}"
            );
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("config")).unwrap();
        crate::data_layout::write_config_file(
            temp.path().join("config/orchestration.toml"),
            "[tool_loop]\n[tools]\n[memory]\n[bridge]\n[http]\npool_idle_timeout_secs = 0\n\
             [logging]\n[telemetry]\n",
        )
        .unwrap();
        let (base, connections, task) = server(handler(hold_open())).await;
        let _ = base;
        let child = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "native_engine::model_http::tests::pool_idle_timeout_must_be_positive",
                "--nocapture",
            ])
            .current_dir(temp.path())
            .env(CHILD, "1")
            .env_remove("DEEPSEEK_CODE_DATA_DIR")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await
            .unwrap();
        let output = format!(
            "{}{}",
            String::from_utf8_lossy(&child.stdout),
            String::from_utf8_lossy(&child.stderr)
        );
        assert!(output.contains("1 passed"), "{output}");
        assert!(child.status.success(), "{output}");
        task.abort();
        assert_eq!(
            connections.load(Ordering::SeqCst),
            0,
            "zero pool_idle_timeout_secs must fail before opening any connection"
        );
    }

    #[tokio::test]
    async fn sequential_requests_after_parallelism_reuse_an_idle_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let accepts = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(tokio::sync::Barrier::new(4));

        let counted = accepts.clone();
        let req_count = requests.clone();
        let gate = barrier.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                counted.fetch_add(1, Ordering::SeqCst);
                let req_count = req_count.clone();
                let gate = gate.clone();
                tokio::spawn(async move {
                    for _ in 0..2 {
                        if !read_request(&mut socket).await {
                            return;
                        }
                        let seq = req_count.fetch_add(1, Ordering::SeqCst);
                        if seq < 4 {
                            gate.wait().await;
                        }
                        if socket
                            .write_all(
                                b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\ncontent-type: text/plain\r\n\r\nok",
                            )
                            .await
                            .is_err()
                        {
                            return;
                        }
                        if socket.flush().await.is_err() {
                            return;
                        }
                    }
                });
            }
        });

        let client = streaming_client(Redirect::None).unwrap();
        let mut parallel = Vec::new();
        for _ in 0..4 {
            let client = client.clone();
            let url = base.clone();
            parallel.push(tokio::spawn(async move {
                client
                    .post(&url)
                    .body("{}")
                    .send()
                    .await
                    .unwrap()
                    .text()
                    .await
                    .unwrap()
            }));
        }
        for handle in parallel {
            assert_eq!(handle.await.unwrap(), "ok");
        }
        let accepts_after_parallel = accepts.load(Ordering::SeqCst);
        assert_eq!(
            accepts_after_parallel, 4,
            "four concurrent HTTP/1.1 requests must open four connections"
        );

        let followup = streaming_client(Redirect::None)
            .unwrap()
            .post(&base)
            .body("{}")
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(followup, "ok");
        assert_eq!(
            accepts.load(Ordering::SeqCst),
            accepts_after_parallel,
            "a sequential request after concurrent work must reuse an idle connection"
        );
        task.abort();
    }
}
