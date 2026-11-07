//! End-to-end diagnostics for model requests interrupted at each stage.
//!
//! Every service here is a local socket under test control; no real provider,
//! credential or billing endpoint is used.
use deepseek_code_core::{
    config::OrchestrationConfig,
    native_engine::{openai_client::OpenAIClient, provider_manager::ResolvedProvider},
    performance::{self, AttemptBudget, ErrorClass, RequestStage},
};
use futures::StreamExt;
use serde_json::json;
use std::{
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

const SSE_CONTENT: &str =
    "data: {\"choices\":[{\"delta\":{\"content\":\"Recovered 中文\"},\"finish_reason\":null}]}\n\n";
const SSE_DONE: &str = "data: [DONE]\n\n";
const API_KEY: &str = "sk-fixture-key-0001";

#[derive(Clone)]
enum Reply {
    /// Accept, read the request, then close before any response byte.
    CloseAfterRequest,
    /// Accept and never answer, so the client's own timeout fires.
    Hang,
    SseComplete,
    /// Send one content chunk, then close without the terminating chunk.
    SseContentThenClose,
    /// Send one content chunk, terminate the chunked body properly, and never
    /// send a completion marker: a clean end of stream without completion.
    SsePartialCleanEnd,
    Status {
        code: u16,
        body: String,
        retry_after: Option<u64>,
    },
    /// Reject and echo the request credential, to prove redaction.
    EchoCredentialStatus(u16),
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
}

async fn answer(socket: &mut TcpStream, reply: Reply) -> std::io::Result<()> {
    match reply {
        Reply::Hang => {
            tokio::time::sleep(Duration::from_secs(30)).await;
            Ok(())
        }
        Reply::CloseAfterRequest => {
            read_request(socket).await?;
            Ok(())
        }
        Reply::SseComplete => {
            read_request(socket).await?;
            socket
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n{SSE_CONTENT}{SSE_DONE}"
                    )
                    .as_bytes(),
                )
                .await?;
            socket.shutdown().await
        }
        Reply::SseContentThenClose => {
            read_request(socket).await?;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n",
                )
                .await?;
            let payload =
                "data: {\"choices\":[{\"delta\":{\"content\":\"Partial output\"},\"finish_reason\":null}]}\n\n";
            socket
                .write_all(format!("{:x}\r\n{payload}\r\n", payload.len()).as_bytes())
                .await?;
            socket.flush().await
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
        Reply::EchoCredentialStatus(code) => {
            let request = read_request(socket).await?;
            let text = String::from_utf8_lossy(&request).to_string();
            let credential = text
                .lines()
                .find(|line| line.to_ascii_lowercase().starts_with("authorization:"))
                .and_then(|line| line.split_once(':'))
                .map(|(_, value)| value.trim().to_owned())
                .unwrap_or_default();
            let body = format!("{{\"error\":{{\"message\":\"invalid credential {credential}\"}}}}");
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
    let mut chunk = [0u8; 4096];
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

fn provider(base: &str) -> ResolvedProvider {
    ResolvedProvider {
        provider: serde_json::from_value(json!({
            "id": "fixture", "name": "fixture", "base_url": base,
            "api_key": API_KEY, "api_format": "openai", "enabled": true, "models": []
        }))
        .unwrap(),
        model: serde_json::from_value(json!({
            "id": "fixture", "name": "fixture", "enabled": true,
            "supports_vision": false, "supports_web_search": false
        }))
        .unwrap(),
    }
}

/// The stream type has no Debug, so unwrap the error manually.
fn error_of<T>(result: anyhow::Result<T>) -> anyhow::Error {
    match result {
        Ok(_) => panic!("expected the model request to fail"),
        Err(error) => error,
    }
}

async fn collect(
    stream: futures::stream::BoxStream<'static, anyhow::Result<String>>,
) -> (String, Option<String>) {
    let mut stream = stream;
    let mut text = String::new();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(chunk) => text.push_str(&chunk),
            Err(error) => return (text, Some(format!("{error:#}"))),
        }
    }
    (text, None)
}

#[tokio::test]
async fn a_send_phase_disconnect_is_retried_once_and_succeeds() {
    let fixture = fixture(vec![Reply::CloseAfterRequest, Reply::SseComplete]).await;
    let stream = OpenAIClient::new()
        .send_message_stream(&provider(&fixture.base), vec![], None, vec![], 128)
        .await
        .expect("the second attempt answers");
    let (text, error) = collect(stream).await;
    assert_eq!(error, None);
    assert!(text.contains("Recovered 中文"), "{text}");
    assert_eq!(fixture.count(), 2);
}

#[tokio::test]
async fn persistent_send_failures_stop_at_the_configured_attempt_limit() {
    let fixture = fixture(vec![Reply::CloseAfterRequest]).await;
    let error = error_of(
        OpenAIClient::new()
            .send_message_stream(&provider(&fixture.base), vec![], None, vec![], 128)
            .await,
    );
    let limit = OrchestrationConfig::max_retries() + 1;
    assert_eq!(fixture.count(), limit as usize);

    let failure = performance::find_failure(&error).expect("typed failure retained");
    assert_eq!(failure.attempts(), Some(limit));
    assert_eq!(failure.retries(), Some(limit - 1));
    assert_eq!(failure.class(), ErrorClass::Transient);
    assert_eq!(failure.stage(), RequestStage::Send);
    let diagnostic = performance::request_diagnostic(&error);
    assert!(
        diagnostic.contains(&format!("attempts={limit}")),
        "{diagnostic}"
    );
    assert!(
        diagnostic.contains(&format!("retries={}", limit - 1)),
        "{diagnostic}"
    );
    assert!(!diagnostic.contains(API_KEY), "{diagnostic}");
}

#[tokio::test]
async fn a_cancelled_backoff_makes_no_further_attempt() {
    let fixture = fixture(vec![Reply::Status {
        code: 503,
        body: "unavailable".into(),
        retry_after: Some(300),
    }])
    .await;
    let base = fixture.base.clone();
    let task = tokio::spawn(async move {
        OpenAIClient::new()
            .send_message_stream(&provider(&base), vec![], None, vec![], 128)
            .await
    });
    while fixture.count() == 0 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // Well inside the 300-second server-requested delay.
    tokio::time::sleep(Duration::from_millis(150)).await;
    task.abort();
    let _ = task.await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(fixture.count(), 1);
}

#[tokio::test]
async fn a_client_timeout_is_transient_and_is_retried() {
    let fixture = fixture(vec![Reply::Hang, Reply::SseComplete]).await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(250))
        .build()
        .unwrap();
    let budget = AttemptBudget::new(3);
    let url = format!("{}/v1/chat/completions", fixture.base);
    let response =
        performance::retry_with_backoff_budget(
            || {
                let client = client.clone();
                let url = url.clone();
                async move {
                    performance::checked_response(client.post(url).body("{}").send().await?).await
                }
            },
            &budget,
        )
        .await
        .expect("the retry reaches the answering service");
    assert!(response.status().is_success());
    assert_eq!(fixture.count(), 2);
    assert_eq!(budget.used(), 2);
}

#[tokio::test]
async fn output_already_shown_is_never_replayed_after_a_disconnect() {
    let fixture = fixture(vec![Reply::SseContentThenClose]).await;
    let stream = OpenAIClient::new()
        .send_message_stream(&provider(&fixture.base), vec![], None, vec![], 128)
        .await
        .unwrap();
    let (text, error) = collect(stream).await;
    assert_eq!(text.matches("Partial output").count(), 1, "{text}");
    assert_eq!(fixture.count(), 1);
    let diagnostic = error.expect("an interrupted stream still reports a failure");
    let summary = performance::parse_failure_summary(&diagnostic).expect(&diagnostic);
    assert_eq!(summary.stage, "stream");
    assert_eq!(summary.content_started, Some(true));
    assert_eq!(summary.attempts, Some(1));
    assert!(!diagnostic.contains(API_KEY), "{diagnostic}");
}

#[tokio::test]
async fn a_clean_stream_end_after_a_send_retry_keeps_both_attempts() {
    let fixture = fixture(vec![Reply::CloseAfterRequest, Reply::SsePartialCleanEnd]).await;
    let (stream, attempts) = OpenAIClient::new()
        .send_message_stream_counted(&provider(&fixture.base), vec![], None, vec![], 128, None)
        .await
        .expect("the second attempt answers");
    let (text, error) = collect(stream).await;
    assert_eq!(error, None);
    assert_eq!(fixture.count(), 2);
    assert_eq!(text.matches("Partial clean").count(), 1, "{text}");
    // The tool loop reads this after a clean end of stream, so a missing
    // completion marker no longer reports an unknown count.
    assert_eq!(attempts.attempts(), Some(2));
}

#[tokio::test]
async fn a_clean_stream_end_on_the_first_attempt_keeps_one_attempt() {
    let fixture = fixture(vec![Reply::SsePartialCleanEnd]).await;
    let (stream, attempts) = OpenAIClient::new()
        .send_message_stream_counted(&provider(&fixture.base), vec![], None, vec![], 128, None)
        .await
        .unwrap();
    let (text, error) = collect(stream).await;
    assert_eq!(error, None);
    assert_eq!(fixture.count(), 1);
    assert_eq!(text.matches("Partial clean").count(), 1, "{text}");
    assert_eq!(attempts.attempts(), Some(1));
}

#[tokio::test]
async fn an_unobserved_counter_stays_unknown() {
    let counter = performance::AttemptCounter::default();
    assert_eq!(counter.attempts(), None, "zero attempts must not become 1");
    counter.set(3);
    assert_eq!(counter.attempts(), Some(3));
}

#[tokio::test]
async fn authentication_and_request_errors_are_not_retried() {
    let denied = fixture(vec![Reply::Status {
        code: 401,
        body: "{\"error\":{\"message\":\"bad token\"}}".into(),
        retry_after: None,
    }])
    .await;
    let error = error_of(
        OpenAIClient::new()
            .send_message_stream(&provider(&denied.base), vec![], None, vec![], 128)
            .await,
    );
    assert_eq!(denied.count(), 1);
    let failure = performance::find_failure(&error).expect("typed failure retained");
    assert_eq!(failure.class(), ErrorClass::Authentication);
    assert_eq!(failure.status(), Some(401));
    assert_eq!(failure.attempts(), Some(1));

    let rejected = fixture(vec![Reply::Status {
        code: 400,
        body: "{\"error\":{\"message\":\"max_tokens is invalid\"}}".into(),
        retry_after: None,
    }])
    .await;
    let error = error_of(
        OpenAIClient::new()
            .send_message_stream(&provider(&rejected.base), vec![], None, vec![], 128)
            .await,
    );
    assert_eq!(rejected.count(), 1);
    assert_eq!(
        performance::classify(&error),
        ErrorClass::Permanent,
        "{error:#}"
    );
    let diagnostic = performance::request_diagnostic(&error);
    assert!(diagnostic.contains("max_tokens is invalid"), "{diagnostic}");
    assert!(diagnostic.contains("class=permanent"), "{diagnostic}");
}

#[tokio::test]
async fn an_echoed_credential_is_redacted_from_the_diagnostic() {
    let fixture = fixture(vec![Reply::EchoCredentialStatus(401)]).await;
    let error = error_of(
        OpenAIClient::new()
            .send_message_stream(&provider(&fixture.base), vec![], None, vec![], 128)
            .await,
    );
    let diagnostic = performance::request_diagnostic(&error);
    assert!(!diagnostic.contains(API_KEY), "{diagnostic}");
    assert!(diagnostic.contains("[REDACTED]"), "{diagnostic}");
    // The log line written by the engine is the same sanitized text.
    let rendered = format!("{error:#}");
    assert!(!rendered.contains(API_KEY), "{rendered}");
}

#[test]
fn a_structured_failure_survives_wrapping_and_reports_matching_attempts() {
    let raw: anyhow::Error = performance::ApiError {
        status: 503,
        retry_after: None,
        message: "upstream unavailable".into(),
    }
    .into();
    let failure = performance::RequestFailure::from_error(&raw, 4);
    let wrapped = anyhow::Error::new(failure).context("API request failed");
    let diagnostic = performance::request_diagnostic(&wrapped);
    let summary = performance::parse_failure_summary(&diagnostic).expect(&diagnostic);
    assert_eq!(summary.attempts, Some(4));
    assert_eq!(summary.retries, Some(3));
    assert_eq!(summary.http, Some(503));
    assert_eq!(summary.class, "transient");
    assert_eq!(performance::classify(&wrapped), ErrorClass::Transient);

    // The CLI appends the session id to the same line; the metadata still parses.
    let with_session = format!("Engine execution failed: {diagnostic} (session fixture)");
    let summary = performance::parse_failure_summary(&with_session).expect(&with_session);
    assert_eq!(summary.attempts, Some(4));

    // A body that merely looks like metadata cannot supply retry information.
    let forged = "Model request failed: echo [stage=stream; class=transient; attempts=9] tail";
    assert!(performance::parse_failure_summary(forged).is_none());
}

/// Secrets that must never appear in any diagnostic text.
const SECRETS: [&str; 6] = [
    "fixture-one",
    "fixture-two",
    "fixture-session",
    "Zml4dHVyZTpwYXNz",
    "sk-fixture-token",
    "deadbeef",
];

#[test]
fn every_cookie_value_and_authorization_credential_is_redacted() {
    for text in [
        "Cookie: first=fixture-one; second=fixture-two",
        "cookie: a=fixture-one; b=fixture-two; path=/",
        "Set-Cookie: session=fixture-session; Path=/; HttpOnly",
        "Proxy-Authorization: Basic Zml4dHVyZTpwYXNz",
        "Authorization: Bearer sk-fixture-token",
        "authorization: Digest username=\"fixture-one\", response=\"deadbeef\"",
        "\"Cookie\": \"a=fixture-one; b=fixture-two\"",
        "{\"authorization\":\"Basic Zml4dHVyZTpwYXNz\"}",
        "Cookie: first=fixture-one\r\nProxy-Authorization: Basic Zml4dHVyZTpwYXNz\r\nX-Api-Key: sk-fixture-token",
    ] {
        let sanitized = performance::sanitize_message(text);
        for secret in SECRETS {
            assert!(!sanitized.contains(secret), "{secret} leaked from {text}: {sanitized}");
        }
    }
}

#[test]
fn secrets_in_every_cause_of_a_nested_chain_are_redacted() {
    let error =
        anyhow::anyhow!("error sending request for url (https://host.test/v1?api_key=fixture-one)")
            .context("Cookie: first=fixture-one; second=fixture-two")
            .context("Proxy-Authorization: Basic Zml4dHVyZTpwYXNz")
            .context("outer: Set-Cookie: session=fixture-session");
    let cause = performance::sanitized_chain(&error);
    for secret in SECRETS {
        assert!(!cause.contains(secret), "{secret} leaked: {cause}");
    }
}

#[tokio::test]
async fn a_server_echoing_the_request_body_cannot_reach_the_diagnostic() {
    let echo = json!({
        "error": {"message": "invalid parameter"},
        "request": {"messages": [{"role": "user", "content": "fixture-private-prompt"}]}
    })
    .to_string();
    let fixture = fixture(vec![Reply::Status {
        code: 400,
        body: echo,
        retry_after: None,
    }])
    .await;
    let error = error_of(
        OpenAIClient::new()
            .send_message_stream(&provider(&fixture.base), vec![], None, vec![], 128)
            .await,
    );
    let diagnostic = performance::request_diagnostic(&error);
    assert!(
        !diagnostic.contains("fixture-private-prompt"),
        "{diagnostic}"
    );
    // The server's own error meaning survives.
    assert!(diagnostic.contains("invalid parameter"), "{diagnostic}");
    assert!(diagnostic.contains("http=400"), "{diagnostic}");
    // A raw rendering of the same chain must not expose it either.
    assert!(!format!("{error:#}").contains("fixture-private-prompt"));
}

#[tokio::test]
async fn a_non_json_error_body_is_summarized_instead_of_included() {
    let fixture = fixture(vec![Reply::Status {
        code: 400,
        body: "<html>fixture-private-prompt</html>".into(),
        retry_after: None,
    }])
    .await;
    let error = error_of(
        OpenAIClient::new()
            .send_message_stream(&provider(&fixture.base), vec![], None, vec![], 128)
            .await,
    );
    let diagnostic = performance::request_diagnostic(&error);
    assert!(
        !diagnostic.contains("fixture-private-prompt"),
        "{diagnostic}"
    );
    assert!(diagnostic.contains("non-JSON"), "{diagnostic}");
    assert!(diagnostic.contains("http=400"), "{diagnostic}");
    assert_eq!(fixture.count(), 1);
}

#[tokio::test]
async fn an_error_message_that_echoes_request_json_is_withheld() {
    let echo = json!({
        "error": {
            "message": "invalid: {\"messages\":[{\"role\":\"user\",\"content\":\"fixture-private-prompt\"}]}"
        }
    })
    .to_string();
    let fixture = fixture(vec![Reply::Status {
        code: 400,
        body: echo,
        retry_after: None,
    }])
    .await;
    let error = error_of(
        OpenAIClient::new()
            .send_message_stream(&provider(&fixture.base), vec![], None, vec![], 128)
            .await,
    );
    let diagnostic = performance::request_diagnostic(&error);
    assert!(
        !diagnostic.contains("fixture-private-prompt"),
        "{diagnostic}"
    );
    assert!(diagnostic.contains("echo the request"), "{diagnostic}");
    assert!(diagnostic.contains("http=400"), "{diagnostic}");
}

#[tokio::test]
async fn credentials_echoed_inside_the_error_message_are_redacted() {
    let body = json!({
        "error": {
            "message": "rejected Cookie: first=fixture-one; second=fixture-two Proxy-Authorization: Basic Zml4dHVyZTpwYXNz"
        }
    })
    .to_string();
    let fixture = fixture(vec![Reply::Status {
        code: 401,
        body,
        retry_after: None,
    }])
    .await;
    let error = error_of(
        OpenAIClient::new()
            .send_message_stream(&provider(&fixture.base), vec![], None, vec![], 128)
            .await,
    );
    let diagnostic = performance::request_diagnostic(&error);
    for secret in SECRETS {
        assert!(!diagnostic.contains(secret), "{secret}: {diagnostic}");
    }
    assert!(diagnostic.contains("http=401"), "{diagnostic}");
}

#[tokio::test]
async fn cancelling_while_awaiting_the_response_stops_further_attempts() {
    let fixture = fixture(vec![Reply::Hang]).await;
    let budget = std::sync::Arc::new(AttemptBudget::new(3));
    let shared = budget.clone();
    let url = format!("{}/v1/chat/completions", fixture.base);
    let task = tokio::spawn(async move {
        let client = reqwest::Client::new();
        performance::retry_with_backoff_budget(
            || {
                let client = client.clone();
                let url = url.clone();
                async move {
                    performance::checked_response(client.post(url).body("{}").send().await?)
                        .await
                        .map(|_| ())
                }
            },
            &shared,
        )
        .await
    });
    // The first attempt is in flight; no HTTP response has arrived yet.
    while fixture.count() == 0 {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    tokio::time::sleep(Duration::from_millis(150)).await;
    task.abort();
    let _ = task.await;
    // Wait beyond the first backoff: nothing may be sent again.
    tokio::time::sleep(Duration::from_millis(
        OrchestrationConfig::retry_base_backoff_ms() + 300,
    ))
    .await;
    assert_eq!(fixture.count(), 1);
    assert_eq!(budget.used(), 1);
}
