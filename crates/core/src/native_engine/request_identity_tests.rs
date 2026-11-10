use super::*;
use crate::native_engine::{
    anthropic_client::{AnthropicClient, AnthropicContent, AnthropicMessage},
    openai_client::{OpenAIClient, OpenAIContent, OpenAIMessage},
    provider_manager::{Provider, ResolvedProvider},
    request_trace::RequestTrace,
};
use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use std::sync::{Arc, Mutex};

type Requests = Arc<Mutex<Vec<(HeaderMap, Value)>>>;

async fn capture(
    State(requests): State<Requests>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> impl axum::response::IntoResponse {
    let retry = headers["x-dcl-retry-count"] == "0";
    requests.lock().unwrap().push((headers, body));
    (
        if retry {
            StatusCode::SERVICE_UNAVAILABLE
        } else {
            StatusCode::BAD_REQUEST
        },
        Json(json!({"error":{"message":"fixture rejection"}})),
    )
}

#[tokio::test]
async fn model_transports_preserve_identity_effort_and_retry_ids() {
    let requests: Requests = Default::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .fallback(post(capture))
        .with_state(requests.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut previous_turn = String::new();
    for (index, (builtin, api, mode)) in [
        (None, "anthropic-messages", "observed"),
        (None, "anthropic-messages", "stream"),
        (None, "anthropic-messages", "single"),
        (None, "openai-completions", "observed"),
        (None, "openai-completions", "single"),
        (Some("anthropic"), "anthropic-messages", "observed"),
        (Some("openai"), "openai-responses", "observed"),
        (Some("opencode-go"), "openai-completions", "observed"),
    ]
    .into_iter()
    .enumerate()
    {
        let p: Provider = serde_json::from_value(json!({"id":"fixture","name":"fixture","base_url":url,"api_key":"test-only","api_format":"openai","builtin_id":builtin,"enabled":true,"models":[{"id":"gpt-6-astra","name":"fixture","enabled":true,"max_tokens":8192,"supports_vision":true,"supports_web_search":false,"api":api,"effort":"xhigh"}]})).unwrap();
        let mut p = ResolvedProvider {
            model: p.models[0].clone(),
            provider: p,
        };
        p.model.effort = Some("xhigh".into());
        let identity =
            TurnIdentity::new(Some("session-child".into()), Some("session-parent".into()));
        let request_id = format!("request-{index}");
        identity
            .scope(async {
                let (tx, _rx) = tokio::sync::mpsc::channel(8);
                let mut trace = RequestTrace::new(json!({"id":request_id}));
                let context = trace.capture_context(tx);
                assert_eq!(trace.started()["sessionId"], "session-child");
                assert_eq!(trace.started()["parentSessionId"], "session-parent");
                let failed = if builtin.is_none() && api == "anthropic-messages" {
                    let client = AnthropicClient::new();
                    let messages = vec![AnthropicMessage {
                        role: "user".into(),
                        content: AnthropicContent::Text("hello".into()),
                    }];
                    match mode {
                        "single" => client
                            .send_message(&p, messages, None, vec![], 8192)
                            .await
                            .is_err(),
                        "stream" => client
                            .send_message_stream(&p, messages, None, vec![], 8192)
                            .await
                            .is_err(),
                        _ => client
                            .send_message_stream_with_thinking_observed(
                                &p,
                                messages,
                                None,
                                vec![],
                                8192,
                                true,
                                Some(&context),
                            )
                            .await
                            .is_err(),
                    }
                } else {
                    let client = OpenAIClient::new();
                    let messages = vec![OpenAIMessage {
                        role: "user".into(),
                        content: OpenAIContent::Text("hello".into()),
                        tool_calls: None,
                        tool_call_id: None,
                        reasoning_content: None,
                        provider_content: None,
                    }];
                    if mode == "single" {
                        client
                            .send_message(&p, messages, None, vec![], 8192)
                            .await
                            .is_err()
                    } else {
                        client
                            .send_message_stream_observed(
                                &p,
                                messages,
                                None,
                                vec![],
                                8192,
                                Some(&context),
                            )
                            .await
                            .is_err()
                    }
                };
                assert!(failed);
                let captured = requests.lock().unwrap();
                let pair = &captured[index * 2..];
                assert_eq!(pair.len(), 2);
                for (attempt, (headers, body)) in pair.iter().enumerate() {
                    // Generic session headers are not sent to any provider.
                    assert!(!headers.contains_key("x-session-id"));
                    assert!(!headers.contains_key("session-id"));
                    assert!(!headers.contains_key("thread-id"));
                    if builtin == Some("opencode-go") {
                        assert_eq!(headers["x-opencode-session"], "session-child");
                    } else {
                        assert!(!headers.contains_key("x-opencode-session"));
                    }
                    assert_eq!(headers["x-dcl-session-id"], "session-child");
                    assert_eq!(headers["x-parent-session-id"], "session-parent");
                    assert_eq!(headers["x-dcl-runtime"], "rust");
                    assert_eq!(headers["x-dcl-retry-count"], attempt.to_string());
                    assert_eq!(headers["x-request-id"], pair[0].0["x-request-id"]);
                    assert_eq!(headers["x-dcl-turn-id"], pair[0].0["x-dcl-turn-id"]);
                    assert_eq!(
                        headers["x-dcl-turn-id"],
                        trace.started()["turnId"].as_str().unwrap()
                    );
                    if mode == "observed" {
                        assert_eq!(headers["x-request-id"], request_id);
                    }
                    if api == "anthropic-messages" {
                        assert_eq!(body["output_config"]["effort"], "xhigh");
                        assert_eq!(body["thinking"]["type"], "adaptive");
                        assert!(body["thinking"].get("budget_tokens").is_none());
                    } else if api == "openai-responses" {
                        assert_eq!(body["reasoning"]["effort"], "xhigh");
                    } else {
                        assert_eq!(body["reasoning_effort"], "xhigh");
                    }
                }
                assert_ne!(pair[0].0["x-dcl-turn-id"], previous_turn);
                previous_turn = pair[0].0["x-dcl-turn-id"].to_str().unwrap().into();
            })
            .await;
    }
    server.abort();
}

#[tokio::test]
async fn simultaneous_sessions_children_and_cancellation_do_not_leak_identity() {
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let mut tasks = Vec::new();
    for session in ["one", "two"] {
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(
            TurnIdentity::new(Some(session.into()), None).scope(async move {
                barrier.wait().await;
                let headers = RequestIdentity::new(None, true).headers();
                assert_eq!(headers["x-opencode-session"], session);
                assert_eq!(headers["x-dcl-session-id"], session);
                // A provider that is not OpenCode never receives a session ID.
                let other = RequestIdentity::new(None, false).headers();
                assert!(!other.contains_key("x-opencode-session"));
                assert!(!other.contains_key("session-id"));
                TurnIdentity::new(Some("child".into()), None)
                    .scope(async {
                        let headers = RequestIdentity::new(None, true).headers();
                        assert_eq!(headers["x-parent-session-id"], session);
                        assert_eq!(headers["x-opencode-session"], "child");
                        assert!(!headers.contains_key("session-id"));
                    })
                    .await;
                assert_eq!(
                    RequestIdentity::new(None, true).headers()["x-opencode-session"],
                    session
                );
            }),
        ));
    }
    for task in tasks {
        task.await.unwrap();
    }
    let _ = tokio::time::timeout(
        std::time::Duration::from_millis(1),
        TurnIdentity::new(Some("cancelled".into()), None).scope(std::future::pending::<()>()),
    )
    .await;
    let headers = RequestIdentity::new(None, true).headers();
    assert!(!headers.contains_key("session-id"));
    // Outside a conversation the routing session falls back to the request ID.
    assert_eq!(headers["x-opencode-session"], headers["x-request-id"]);
    assert!(!RequestIdentity::new(None, false)
        .headers()
        .contains_key("x-opencode-session"));
}

#[test]
fn only_open_code_providers_are_routed_by_session() {
    fn provider(builtin: Option<&str>, base_url: &str) -> ResolvedProvider {
        let p: Provider = serde_json::from_value(json!({
            "id": "fixture", "name": "fixture", "base_url": base_url,
            "api_key": "test-only", "api_format": "openai", "builtin_id": builtin,
            "enabled": true,
            "models": [{"id":"fixture-model","name":"fixture","enabled":true,"supports_web_search":false}]
        }))
        .unwrap();
        ResolvedProvider {
            model: p.models[0].clone(),
            provider: p,
        }
    }
    assert!(is_opencode(&provider(
        Some("opencode-go"),
        "https://example.test/v1"
    )));
    assert!(is_opencode(&provider(
        Some("opencode"),
        "https://example.test/v1"
    )));
    // A custom provider that points at the OpenCode host is recognized too.
    assert!(is_opencode(&provider(
        None,
        "https://opencode.ai/inference/go/openai/v1"
    )));
    assert!(is_opencode(&provider(None, "https://api.opencode.ai/v1")));
    assert!(!is_opencode(&provider(
        Some("openai"),
        "https://api.openai.com/v1"
    )));
    assert!(!is_opencode(&provider(None, "https://example.com/v1")));
    // A host that merely contains the name is not OpenCode.
    assert!(!is_opencode(&provider(None, "https://not-opencode.ai/v1")));
    assert!(!is_opencode(&provider(
        None,
        "https://opencode.ai.example.com/v1"
    )));
}

#[test]
fn anthropic_effort_preserves_schema_and_legacy_thinking() {
    use crate::native_engine::anthropic_client::apply_effort;
    let mut body = json!({"output_config":{"format":{"type":"json_schema"}}});
    apply_effort(&mut body, "xhigh", "gpt-6-astra", 8192).unwrap();
    assert_eq!(body["output_config"]["format"]["type"], "json_schema");
    assert_eq!(body["output_config"]["effort"], "xhigh");
    let mut legacy = json!({});
    apply_effort(&mut legacy, "high", "claude-sonnet-4-5", 8192).unwrap();
    assert_eq!(legacy["thinking"]["budget_tokens"], 8191);
    assert!(legacy.get("output_config").is_none());
    assert!(apply_effort(&mut json!({}), "invalid", "claude-opus-4-6", 8192).is_err());
    apply_effort(&mut legacy, "off", "claude-sonnet-4-5", 8192).unwrap();
    assert_eq!(legacy["thinking"]["type"], "disabled");
}
