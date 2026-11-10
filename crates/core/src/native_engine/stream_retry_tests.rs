use super::*;
use crate::native_engine::{
    anthropic_client::AnthropicClient, openai_client::OpenAIClient,
    provider_manager::ResolvedProvider, request_identity::TurnIdentity,
};
use axum::{body::Body, http::HeaderMap, routing::post, Json, Router};
use serde_json::json;
use std::sync::{Arc, Mutex};

#[path = "../../tests/support/manual_clock.rs"]
mod manual_clock;

const ANTHROPIC_EMPTY: &str = "data: {\"type\":\"message_start\",\"message\":{\"id\":\"empty\",\"content\":[],\"usage\":{\"input_tokens\":0,\"output_tokens\":0}}}\n\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n";
const OPENAI_EMPTY: &str = "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}\n\n";

type Captured = Arc<Mutex<Vec<(HeaderMap, Value)>>>;

async fn fixture(
    responses: Vec<(String, bool)>,
) -> (ResolvedProvider, Captured, tokio::task::JoinHandle<()>) {
    let requests: Captured = Default::default();
    let captured = requests.clone();
    let app = Router::new().fallback(post(move |headers: HeaderMap, Json(body): Json<Value>| {
        let mut requests = captured.lock().unwrap();
        let (text, reset) = responses[requests.len().min(responses.len() - 1)].clone();
        requests.push((headers, body));
        async move {
            let stream = async_stream::stream! {
                yield Ok::<_, std::io::Error>(text);
                if reset {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    yield Err(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "fixture reset"));
                }
            };
            ([("content-type", "text/event-stream")], Body::from_stream(stream))
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let provider = ResolvedProvider {
        provider: serde_json::from_value(json!({"id":"fixture","name":"fixture","base_url":base,"api_key":"fixture-only","api_format":"openai","enabled":true,"models":[]})).unwrap(),
        model: serde_json::from_value(json!({"id":"fixture","name":"fixture","enabled":true,"supports_web_search":false})).unwrap(),
    };
    (provider, requests, server)
}

async fn client(
    provider: &ResolvedProvider,
    kind: usize,
) -> Pin<Box<dyn Stream<Item = Result<String>> + Send>> {
    match kind {
        0 => OpenAIClient::new()
            .send_message_stream(provider, vec![], None, vec![], 128)
            .await
            .unwrap(),
        1 => AnthropicClient::new()
            .send_message_stream(provider, vec![], None, vec![], 128)
            .await
            .unwrap(),
        _ => AnthropicClient::new()
            .send_message_stream_with_thinking(provider, vec![], None, vec![], 128, false)
            .await
            .unwrap(),
    }
}

async fn collect(
    mut stream: Pin<Box<dyn Stream<Item = Result<String>> + Send>>,
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

#[tokio::test]
async fn empty_disconnect_recovers_without_duplicate_events_or_changed_identity() {
    for kind in 0..3 {
        for reset in [false, true] {
            let prelude = if kind == 0 {
                OPENAI_EMPTY
            } else {
                ANTHROPIC_EMPTY
            };
            let completed = if kind == 0 {
                "data: {\"choices\":[{\"delta\":{\"content\":\"Recovered 中文\"},\"finish_reason\":\"stop\"}]}\n\n"
            } else {
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Recovered 中文\"}}\n\ndata: {\"type\":\"message_stop\"}\n\n"
            };
            let successful = format!("{prelude}{completed}");
            let (provider, requests, server) =
                fixture(vec![(prelude.into(), reset), (successful.clone(), false)]).await;
            let stream = TurnIdentity::new(Some("resumed-session".into()), None)
                .scope(client(&provider, kind))
                .await;
            let (text, error) = collect(stream).await;
            assert!(error.is_none(), "{error:?}");
            assert_eq!(text, successful);
            let requests = requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert_eq!(requests[0].1, requests[1].1);
            for (index, (headers, _)) in requests.iter().enumerate() {
                // This provider is not OpenCode, so no session ID is disclosed.
                assert!(!headers.contains_key("x-opencode-session"));
                assert!(!headers.contains_key("session-id"));
                assert_eq!(headers["x-request-id"], requests[0].0["x-request-id"]);
                assert_eq!(headers["x-dcl-turn-id"], requests[0].0["x-dcl-turn-id"]);
                assert_eq!(headers["x-dcl-retry-count"], index.to_string());
            }
            server.abort();
        }
    }
}

#[tokio::test(start_paused = true)]
async fn empty_disconnect_is_bounded_and_can_be_cancelled_during_backoff() {
    use crate::{native_engine::tool_loop::EngineEvent, performance::retry_events};

    manual_clock::run(async {
        let (provider, requests, server) = fixture(vec![(ANTHROPIC_EMPTY.into(), false)]).await;
        let (tx, mut events) = tokio::sync::mpsc::channel(32);
        let pending = retry_events::scope(tx, async { collect(client(&provider, 2).await).await });
        tokio::pin!(pending);
        let started = tokio::time::Instant::now();
        for attempt in 1..8 {
            let event = loop {
                tokio::select! {
                    biased;
                    result = &mut pending => panic!("stream ended before retry {attempt}: {result:?}"),
                    event = events.recv() => match event.expect("retry events remain open") {
                        EngineEvent::RequestAttempt(event) if event["outcome"] == "retry_wait" => break event,
                        _ => {},
                    },
                }
            };
            let delay = Duration::from_secs(2 << (attempt - 1));
            assert_eq!(event["attempts"], attempt);
            assert_eq!(event["waitMs"], delay.as_millis() as u64);
            assert_eq!(requests.lock().unwrap().len(), attempt as usize);
            tokio::time::advance(delay - Duration::from_millis(1)).await;
            assert!(futures::poll!(&mut pending).is_pending());
            assert_eq!(requests.lock().unwrap().len(), attempt as usize);
            tokio::time::advance(Duration::from_millis(1)).await;
        }
        let (text, error) = pending.await;
        assert_eq!(started.elapsed(), Duration::from_secs(254));
        assert_eq!(requests.lock().unwrap().len(), 8);
        assert_eq!(text, ANTHROPIC_EMPTY);
        assert!(error.unwrap().contains("attempts=8"));
        server.abort();

        let (provider, requests, server) = fixture(vec![(ANTHROPIC_EMPTY.into(), false)]).await;
        let (tx, mut events) = tokio::sync::mpsc::channel(32);
        let mut stream = retry_events::scope(tx, client(&provider, 2)).await;
        let started = tokio::time::Instant::now();
        {
            let next = stream.next();
            tokio::pin!(next);
            loop {
                tokio::select! {
                    biased;
                    result = &mut next => panic!("empty stream exposed output before retry: {result:?}"),
                    event = events.recv() => {
                        if let EngineEvent::RequestAttempt(event) = event.expect("retry events remain open") {
                            if event["outcome"] == "retry_wait" {
                                assert_eq!(event["waitMs"], 2000);
                                break;
                            }
                        }
                    },
                }
            }
            assert_eq!(started.elapsed(), Duration::ZERO);
            tokio::time::advance(Duration::from_millis(50)).await;
            assert!(futures::poll!(&mut next).is_pending());
        }
        drop(stream);
        tokio::time::advance(Duration::from_millis(2100)).await;
        assert_eq!(requests.lock().unwrap().len(), 1);
        server.abort();
    })
    .await;
}

#[tokio::test]
async fn content_tools_completion_errors_and_unknown_data_are_never_replayed() {
    for suffix in [
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"call\",\"name\":\"Write\",\"input\":{}}}\n\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"partial\"}}\n\n",
        "data: {\"type\":\"message_stop\"}\n\n",
        "data: {\"type\":\"error\",\"error\":{\"message\":\"fixture rejected\"}}\n\n",
        "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0}]}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"partial\"}}]}\n\n",
        "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
        "data: [DONE]\n\n",
        "data: {\"unknown\":true}\n\n",
        "data: {\"choices\":[],\"provider_content\":{\"content\":[{\"type\":\"toolCall\"}]}}\n\n",
        "data: {\"unfinished\":",
    ] {
        let source = format!("{ANTHROPIC_EMPTY}{suffix}");
        let (provider, requests, server) = fixture(vec![(source.clone(), false)]).await;
        let (text, error) = collect(client(&provider, 2).await).await;
        assert_eq!(text, source);
        assert!(error.is_none());
        assert_eq!(requests.lock().unwrap().len(), 1, "{suffix}");
        server.abort();
    }
    let source = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n";
    let (provider, requests, server) = fixture(vec![(source.into(), true)]).await;
    let (text, error) = collect(client(&provider, 0).await).await;
    assert_eq!(text, source);
    assert!(error.is_some());
    assert_eq!(requests.lock().unwrap().len(), 1);
    server.abort();
}
