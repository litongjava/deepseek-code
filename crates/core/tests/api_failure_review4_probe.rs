//! Regression for the fourth review: stream error events, correlation IDs,
//! permanent quota errors, remote header text and stream termination status.
//!
//! Every service is a local socket; no credential, billing endpoint or provider
//! is contacted.
use deepseek_code_core::{
    native_engine::{
        anthropic_client::AnthropicClient, openai_client::OpenAIClient,
        provider_manager::ResolvedProvider, request_trace::RequestTrace,
    },
    performance::{self, ErrorClass, RequestStage},
};
use futures::StreamExt;
use serde_json::json;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

const MARKER: &str = "PRIVATE_REVIEW4_PROMPT";
const SERVER_ID: &str = "11111111-2222-4333-8444-555555555555";
const CRLF: &str = "\r\n";

#[derive(Clone)]
enum Reply {
    /// HTTP 200 with an SSE `error` event carrying the marker.
    SseError,
    /// HTTP 200, one content chunk, then a clean end without completion.
    PartialEof,
    /// 429 with the given recognized identifier.
    Quota { code: &'static str },
    /// 500 with a correlation ID in both the header and the JSON body.
    ServerErrorWithId,
    /// 400 whose Content-Type embeds the marker.
    EchoContentType,
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

impl Fixture {
    fn count(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

async fn respond(
    socket: &mut TcpStream,
    status: &str,
    content_type: &str,
    extra: &str,
    body: &str,
) -> std::io::Result<()> {
    socket
        .write_all(
            format!(
                "HTTP/1.1 {status}{CRLF}content-type: {content_type}{CRLF}content-length: {}{CRLF}{extra}connection: close{CRLF}{CRLF}{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await?;
    socket.shutdown().await
}

async fn chunked(socket: &mut TcpStream, payloads: &[String]) -> std::io::Result<()> {
    socket
        .write_all(
            format!(
                "HTTP/1.1 200 OK{CRLF}content-type: text/event-stream{CRLF}transfer-encoding: chunked{CRLF}connection: close{CRLF}{CRLF}"
            )
            .as_bytes(),
        )
        .await?;
    for payload in payloads {
        socket
            .write_all(format!("{:x}{CRLF}{payload}{CRLF}", payload.len()).as_bytes())
            .await?;
    }
    socket.write_all(format!("0{CRLF}{CRLF}").as_bytes()).await?;
    socket.flush().await
}

async fn answer(socket: &mut TcpStream, reply: Reply) -> std::io::Result<()> {
    read_request(socket).await?;
    match reply {
        Reply::SseError => {
            let event = json!({
                "type": "error",
                "error": {"type": "invalid_request_error", "message": MARKER}
            });
            chunked(socket, &[format!("data: {event}\n\n")]).await
        }
        Reply::PartialEof => {
            let event = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Visible partial\"},\"finish_reason\":null}]}\n\n";
            chunked(socket, &[event.to_owned()]).await
        }
        Reply::Quota { code } => {
            let body = json!({
                "error": {"message": "insufficient quota", "code": code, "type": code}
            })
            .to_string();
            respond(socket, "429 Too Many Requests", "application/json", "", &body).await
        }
        Reply::ServerErrorWithId => {
            let body = json!({
                "error": {
                    "message": format!("server error request_id: {SERVER_ID}"),
                    "type": "internal_error"
                },
                "request_id": SERVER_ID
            })
            .to_string();
            let extra = format!("x-request-id: {SERVER_ID}{CRLF}");
            respond(socket, "500 Internal Server Error", "application/json", &extra, &body).await
        }
        Reply::EchoContentType => {
            let content_type = format!("application/x-{MARKER}");
            respond(socket, "400 Bad Request", &content_type, "", "{}").await
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

fn provider(base: &str, format: &str) -> ResolvedProvider {
    ResolvedProvider {
        provider: serde_json::from_value(json!({
            "id": "fixture", "name": "fixture", "base_url": base,
            "api_key": "fixture-review4-only", "api_format": format,
            "enabled": true, "models": []
        }))
        .unwrap(),
        model: serde_json::from_value(json!({
            "id": "fixture", "name": "fixture", "enabled": true,
            "supports_vision": false, "supports_web_search": false
        }))
        .unwrap(),
    }
}

fn error_of<T>(result: anyhow::Result<T>) -> anyhow::Error {
    match result {
        Ok(_) => panic!("expected the model request to fail"),
        Err(error) => error,
    }
}

async fn collect(
    mut stream: futures::stream::BoxStream<'static, anyhow::Result<String>>,
) -> (String, Option<String>) {
    let mut text = String::new();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(chunk) => text.push_str(&chunk),
            Err(error) => return (text, Some(format!("{error:#}"))),
        }
    }
    (text, None)
}

fn events(text: &str) -> Vec<serde_json::Value> {
    text.split("\n\n")
        .filter(|payload| !payload.trim().is_empty())
        .filter_map(|payload| serde_json::from_str(payload.trim_start_matches("data: ")).ok())
        .collect()
}

#[tokio::test]
async fn an_openai_stream_error_is_reported_as_a_safe_structured_failure() {
    let fixture = fixture(Reply::SseError).await;
    let mut trace = RequestTrace::new(json!({"id":"request","startedAt":0}));
    let stream = OpenAIClient::new()
        .send_message_stream(&provider(&fixture.base, "openai"), vec![], None, vec![], 128)
        .await
        .unwrap();
    let (text, _) = collect(stream).await;
    for event in events(&text) {
        trace.observe_openai(&event);
    }
    let snapshot = trace.finish_observed("completed", None, Some(1), Some(200));
    assert_eq!(snapshot["status"], "error");
    let error = snapshot["error"].as_str().unwrap();
    assert!(!error.contains(MARKER), "{error}");
    assert!(error.contains("invalid_request_error"), "{error}");
    assert!(error.contains("http=200"), "{error}");
    assert_eq!(snapshot["failure"]["stage"], "stream");
    assert_eq!(snapshot["failure"]["class"], "permanent");
    assert_eq!(snapshot["failure"]["attempts"], 1);
    assert_eq!(fixture.count(), 1);
}

#[tokio::test]
async fn an_anthropic_stream_error_is_reported_as_a_safe_structured_failure() {
    let fixture = fixture(Reply::SseError).await;
    let mut trace = RequestTrace::new(json!({"id":"request","startedAt":0}));
    let stream = AnthropicClient::new()
        .send_message_stream(&provider(&fixture.base, "anthropic"), vec![], None, vec![], 128)
        .await
        .unwrap();
    let (text, _) = collect(stream).await;
    for event in events(&text) {
        trace.observe_anthropic(&event);
    }
    let snapshot = trace.finish_observed("completed", None, Some(1), Some(200));
    assert_eq!(snapshot["status"], "error");
    let error = snapshot["error"].as_str().unwrap();
    assert!(!error.contains(MARKER), "{error}");
    assert!(error.contains("http=200"), "{error}");
    assert_eq!(snapshot["failure"]["stage"], "stream");
    assert_eq!(fixture.count(), 1);
}

#[tokio::test]
async fn a_correlation_id_survives_the_reduction_of_the_error_text() {
    let fixture = fixture(Reply::ServerErrorWithId).await;
    let budget = performance::AttemptBudget::new(2);
    let url = format!("{}/v1/chat/completions", fixture.base);
    let result = performance::retry_with_backoff_budget(
        || {
            let url = url.clone();
            async move {
                let client = reqwest::Client::new();
                performance::checked_response(client.post(url).body("{}").send().await?).await
            }
        },
        &budget,
    )
    .await;
    let error = error_of(result);
    let failure = performance::find_failure(&error).expect("typed failure retained");
    assert_eq!(failure.status(), Some(500));
    // The ID is read before the free text is reduced, so it survives.
    assert_eq!(failure.server_request_id(), Some(SERVER_ID));
    assert!(
        !failure.diagnostic().contains(MARKER),
        "{}",
        failure.diagnostic()
    );
    assert_eq!(failure.attempts(), Some(2));
    assert_eq!(fixture.count(), 2);
}

#[tokio::test]
async fn a_permanent_quota_error_is_not_retried() {
    let fixture = fixture(Reply::Quota {
        code: "insufficient_quota",
    })
    .await;
    let error = error_of(
        OpenAIClient::new()
            .send_message_stream(&provider(&fixture.base, "openai"), vec![], None, vec![], 128)
            .await,
    );
    let failure = performance::find_failure(&error).expect("typed failure retained");
    assert_eq!(failure.status(), Some(429));
    assert_eq!(failure.class(), ErrorClass::Permanent);
    assert_eq!(failure.attempts(), Some(1));
    assert_eq!(
        fixture.count(),
        1,
        "a permanent quota error must be requested once"
    );
}

#[tokio::test]
async fn a_recognised_quota_identifier_allows_one_attempt_only() {
    let fixture = fixture(Reply::Quota {
        code: "quota_exceeded",
    })
    .await;
    let error = error_of(
        OpenAIClient::new()
            .send_message_stream(&provider(&fixture.base, "openai"), vec![], None, vec![], 128)
            .await,
    );
    let failure = performance::find_failure(&error).unwrap();
    assert_eq!(failure.retries(), Some(0));
    assert_eq!(fixture.count(), 1);
}

#[tokio::test]
async fn a_normal_rate_limit_stays_retryable() {
    let fixture = fixture(Reply::Quota {
        code: "rate_limit_error",
    })
    .await;
    let budget = performance::AttemptBudget::new(3);
    let url = format!("{}/v1/chat/completions", fixture.base);
    let result = performance::retry_with_backoff_budget(
        || {
            let url = url.clone();
            async move {
                let client = reqwest::Client::new();
                performance::checked_response(client.post(url).body("{}").send().await?).await
            }
        },
        &budget,
    )
    .await;
    let error = error_of(result);
    let failure = performance::find_failure(&error).unwrap();
    assert_eq!(failure.class(), ErrorClass::RateLimit);
    assert_eq!(budget.used(), 3, "an ordinary limit keeps its retries");
    assert_eq!(fixture.count(), 3);
}

#[tokio::test]
async fn a_remote_content_type_cannot_carry_text_into_the_diagnostic() {
    let fixture = fixture(Reply::EchoContentType).await;
    let error = error_of(
        OpenAIClient::new()
            .send_message_stream(&provider(&fixture.base, "openai"), vec![], None, vec![], 128)
            .await,
    );
    let diagnostic = performance::request_diagnostic(&error);
    assert!(!diagnostic.contains(MARKER), "{diagnostic}");
    assert!(diagnostic.contains("unknown"), "{diagnostic}");
    assert_eq!(
        performance::safe_media_type("application/x-PRIVATE"),
        "unknown"
    );
    assert_eq!(
        performance::safe_media_type("Application/JSON; charset=utf-8"),
        "application/json"
    );
    assert_eq!(performance::safe_media_type(""), "unspecified");
}

#[tokio::test]
async fn a_partial_stream_without_completion_is_not_a_cancellation() {
    let fixture = fixture(Reply::PartialEof).await;
    let stream = OpenAIClient::new()
        .send_message_stream(&provider(&fixture.base, "openai"), vec![], None, vec![], 128)
        .await
        .unwrap();
    let (text, error) = collect(stream).await;
    assert_eq!(error, None, "a clean end of body is not a transport error");
    assert_eq!(text.matches("Visible partial").count(), 1, "{text}");
    assert_eq!(
        fixture.count(),
        1,
        "content already shown must not be replayed"
    );
}

#[test]
fn an_exhausted_stream_keeps_a_known_status() {
    let failure = performance::RequestFailure::stream_exhausted(3, Some(200));
    assert_eq!(failure.stage(), RequestStage::Stream);
    assert_eq!(failure.status(), Some(200));
    assert_eq!(failure.attempts(), Some(3));
    assert_eq!(failure.retries(), Some(2));
    // An unobserved status stays unknown instead of becoming 200.
    assert_eq!(
        performance::RequestFailure::stream_exhausted(1, None).status(),
        None
    );
}

#[test]
fn recognised_identifiers_map_to_the_expected_classes() {
    for (code, expected) in [
        ("invalid_request_error", ErrorClass::Permanent),
        ("authentication_error", ErrorClass::Authentication),
        ("rate_limit_error", ErrorClass::RateLimit),
        ("overloaded_error", ErrorClass::Transient),
        ("context_length_exceeded", ErrorClass::ContextLimit),
        ("insufficient_quota", ErrorClass::Permanent),
    ] {
        let value = json!({"error": {"code": code, "message": MARKER}});
        let failure = performance::RequestFailure::remote_event(&value, Some(1), Some(200), false);
        assert_eq!(failure.class(), expected, "{code}");
        assert!(
            !failure.cause().contains(MARKER),
            "{code}: {}",
            failure.cause()
        );
    }
}
