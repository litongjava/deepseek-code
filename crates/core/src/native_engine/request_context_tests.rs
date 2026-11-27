use super::*;
use crate::native_engine::{
    anthropic_client::{AnthropicClient, AnthropicContent, AnthropicMessage},
    openai_client::{OpenAIClient, OpenAIContent, OpenAIMessage},
    provider_manager::{Provider, ResolvedProvider},
    request_trace::RequestTrace,
};
use axum::{extract::State, routing::post, Json, Router};
use serde_json::json;

fn provider(id: Option<&str>, api: &str, url: &str) -> ResolvedProvider {
    let p: Provider = serde_json::from_value(json!({"id":"fixture","name":"fixture","base_url":url,"api_key":"CONTEXT_PRIVATE_KEY","api_format":"openai","builtin_id":id,"enabled":true,"models":[{"id":"fixture-model","name":"fixture","enabled":true,"max_tokens":8192,"context_window":128000,"supports_vision":true,"supports_web_search":false,"api":api}]})).unwrap();
    ResolvedProvider {
        model: p.models[0].clone(),
        provider: p,
    }
}
fn user() -> OpenAIMessage {
    OpenAIMessage {
        role: "user".into(),
        content: OpenAIContent::Text("CONTEXT_PRIVATE_PROMPT".into()),
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        provider_content: Some(json!({"private":"STRIPPED_PROVIDER_PAYLOAD"})),
    }
}
fn tools() -> Vec<crate::tools::ToolDefinition> {
    vec![crate::tools::ToolDefinition {
        summary: None,
        name: "Read".into(),
        description: "CONTEXT_PRIVATE_TOOL".into(),
        input_schema: json!({"type":"object","properties":{"path":{"type":"string"}}}),
    }]
}

#[test]
fn categories_cover_messages_calls_results_schema_and_unknown_media() {
    let body = json!({"messages":[{"role":"system","content":"system"},{"role":"developer","content":"developer"},{"role":"assistant","content":null,"tool_calls":[{"function":{"arguments":"LONG_TOOL_ARGUMENTS","name":"Read"}}]},{"role":"tool","content":"LONG_TOOL_RESULT"}],"tools":[{"name":"Read","parameters":{"type":"object"}}],"response_format":{"type":"json_schema","schema":{"type":"object"}},"max_completion_tokens":123});
    let usage = ContextUsage::from_body("openai-completions", &body, Some(1000), 200);
    let size = |v: &Value| serde_json::to_vec(v).unwrap().len() as u64;
    assert_eq!(
        usage.instructions.serialized_utf8_bytes,
        size(&body["messages"][0]) + size(&body["messages"][1])
    );
    assert_eq!(
        usage.history.serialized_utf8_bytes,
        size(&body["messages"][2]) + size(&body["messages"][3])
    );
    assert_eq!(usage.tools.serialized_utf8_bytes, size(&body["tools"]));
    assert_eq!(
        usage.output_schema.serialized_utf8_bytes,
        size(&body["response_format"])
    );
    assert_eq!(usage.sent_output_reserve, Some(123));
    assert_eq!(usage.estimated_input_tokens, Some(usage.text_subtotal()));
    for block in [
        json!({"type":"image_url","image_url":{"url":format!("data:image/png;base64,{}","x".repeat(10000))}}),
        json!({"inlineData":{"data":"BASE64","mimeType":"image/png"}}),
        json!({"image":{"source":{"bytes":"BINARY"}}}),
        json!({"type":"reasoning","encrypted_content":"OPAQUE"}),
    ] {
        let value = ContextUsage::from_body(
            "openai-completions",
            &json!({"messages":[{"role":"user","content":[{"type":"text","text":"question"},block]}]}),
            None,
            100,
        );
        assert_eq!(value.unknown_parts(), 1);
        assert_eq!(value.estimated_input_tokens, None);
        assert!(value.history.serialized_utf8_bytes < 200);
        assert_eq!(value.configured_context_window, None);
    }
    let encoded = serde_json::to_string(&usage).unwrap();
    assert!(!encoded.contains("LONG_TOOL"));
    assert!(!encoded.contains("developer\""));
}

async fn capture(
    State(requests): State<Arc<Mutex<Vec<Value>>>>,
    Json(body): Json<Value>,
) -> impl axum::response::IntoResponse {
    requests.lock().unwrap().push(body);
    // A rejected request still has accounting for the actual submitted body.
    (
        axum::http::StatusCode::BAD_REQUEST,
        Json(json!({"error":{"message":"fixture rejection"}})),
    )
}

#[tokio::test]
async fn transport_accounting_matches_final_mock_wire_requests_for_all_protocols() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .fallback(post(capture))
        .with_state(requests.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    for (builtin, protocol) in [
        (None, "openai-completions"),
        (None, "anthropic-messages"),
        (Some("openai"), "openai-responses"),
        (Some("anthropic"), "anthropic-messages"),
        (Some("google"), "google-generative-ai"),
        (Some("amazon-bedrock"), "bedrock-converse-stream"),
        (Some("openai-codex"), "openai-codex-responses"),
        (Some("mistral"), "mistral-conversations"),
    ] {
        let mut p = provider(builtin, protocol, &url);
        if builtin == Some("openai-codex") {
            use base64::Engine;
            p.provider.api_key = format!(
                "header.{}.signature",
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
                    json!({"https://api.openai.com/auth":{"chatgpt_account_id":"fixture-account"}})
                        .to_string()
                )
            );
        }
        let context = ContextCapture::default();
        let schema = (!matches!(protocol, "google-generative-ai" | "bedrock-converse-stream")).then(|| json!({"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false}));
        let result = if builtin.is_none() && protocol == "anthropic-messages" {
            AnthropicClient::new()
                .with_response_schema(schema.clone())
                .send_message_stream_with_thinking_observed(
                    &p,
                    vec![AnthropicMessage {
                        role: "user".into(),
                        content: AnthropicContent::Text("CONTEXT_PRIVATE_PROMPT".into()),
                    }],
                    Some("CONTEXT_PRIVATE_SYSTEM"),
                    tools(),
                    2048,
                    false,
                    Some(&context),
                )
                .await
        } else {
            OpenAIClient::new()
                .with_response_schema(schema.clone())
                .send_message_stream_observed(
                    &p,
                    vec![user()],
                    Some("CONTEXT_PRIVATE_SYSTEM"),
                    tools(),
                    2048,
                    Some(&context),
                )
                .await
        };
        assert!(result.is_err(), "{protocol}");
        let body = requests.lock().unwrap().last().unwrap().clone();
        let actual = context.snapshot().expect(protocol);
        assert!(actual.history.estimated_tokens > 0, "{protocol}");
        assert!(actual.instructions.estimated_tokens > 0, "{protocol}");
        assert!(actual.tools.estimated_tokens > 0, "{protocol}");
        assert_eq!(actual.configured_context_window, Some(128000));
        assert!(!body.to_string().contains("STRIPPED_PROVIDER_PAYLOAD"));
        if builtin == Some("openai-codex") {
            assert!(body.get("max_output_tokens").is_none());
            assert_eq!(actual.sent_output_reserve, None);
            assert_eq!(actual.requested_output_budget, 2048);
        } else {
            assert_eq!(actual.sent_output_reserve, Some(2048), "{protocol}");
        }
        let tool_body = if protocol == "bedrock-converse-stream" {
            &body["toolConfig"]
        } else {
            &body["tools"]
        };
        assert_eq!(
            actual.tools.serialized_utf8_bytes,
            serde_json::to_vec(tool_body).unwrap().len() as u64
        );
        if schema.is_some() {
            assert!(actual.output_schema.estimated_tokens > 0, "{protocol}");
        }
        let serialized = serde_json::to_string(&actual).unwrap();
        for secret in ["CONTEXT_PRIVATE", "fixture-account", "http://", "Read"] {
            assert!(!serialized.contains(secret), "{protocol}: {secret}");
        }
    }
    assert_eq!(requests.lock().unwrap().len(), 8);
    server.abort();
}

#[tokio::test]
async fn trace_keeps_metadata_on_completion_failure_and_cancelled_network_wait() {
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let mut trace = RequestTrace::new(json!({"id":"parent-request","source":"native_engine"}));
    let capture = trace.capture_context(tx);
    assert!(trace.started().get("contextUsage").is_none());
    capture
        .record(
            "openai-completions",
            &json!({"messages":[{"role":"user","content":"PRIVATE"}],"max_tokens":200}),
            Some(1000),
            200,
        )
        .await;
    let event = rx.recv().await.unwrap();
    let super::super::tool_loop::EngineEvent::RequestSnapshot(snapshot) = event else {
        panic!("wrong event")
    };
    assert_eq!(snapshot["id"], "parent-request");
    assert_eq!(snapshot["contextUsage"]["sent_output_reserve"], 200);
    assert!(snapshot.get("inputTokens").is_none());
    let snapshot = trace.finish("error", Some("transport failed"));
    assert_eq!(snapshot["contextUsage"]["sent_output_reserve"], 200);
    assert!(snapshot["inputTokens"].is_null());
    assert!(!snapshot.to_string().contains("PRIVATE"));
}
