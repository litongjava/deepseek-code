use super::*;
use crate::native_engine::{
    openai_client::{OpenAIClient, OpenAIContent},
    provider_manager::Provider,
};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    routing::post,
    Json, Router,
};
use futures::StreamExt;
use serde_json::json;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

fn provider(id: &str, api: &str, url: &str) -> ResolvedProvider {
    let p: Provider = serde_json::from_value(json!({"id":"test","name":"test","base_url":url,"api_key":"fixture-only","api_format":"openai","builtin_id":id,"enabled":true,"models":[{"id":"fixture-model","name":"fixture","enabled":true,"max_tokens":8192,"supports_vision":true,"supports_web_search":false,"api":api}]})).unwrap();
    ResolvedProvider {
        model: p.models[0].clone(),
        provider: p,
    }
}
fn user() -> OpenAIMessage {
    OpenAIMessage {
        role: "user".into(),
        content: OpenAIContent::Text("hello".into()),
        tool_calls: None,
        tool_call_id: None,
        reasoning_content: None,
        provider_content: None,
    }
}

#[test]
fn requests_preserve_protocol_and_reasoning_replay() {
    let p = provider("openai", "openai-responses", "http://localhost/v1");
    let mut assistant = user();
    assistant.role = "assistant".into();
    assistant.provider_content = Some(
        json!({"api":"openai-responses","provider":"test","model":"fixture-model","items":[{"type":"reasoning","id":"rs_1","encrypted_content":"opaque"},{"type":"function_call","call_id":"call_1","name":"read","arguments":"{}"}]}),
    );
    let body = messages::request(
        "openai-responses",
        &p,
        &Value::Null,
        vec![user(), assistant],
        Some("system"),
        vec![],
        4000,
    )
    .unwrap();
    assert_eq!(body["input"][1]["encrypted_content"], "opaque");
    assert_eq!(body["input"][2]["call_id"], "call_1");
    assert_eq!(body["max_output_tokens"], 4000);
    assert_eq!(body["store"], false);
    let mut p = provider("deepseek", "openai-completions", "http://localhost");
    p.model.effort = Some("high".into());
    let body = messages::request(
        "openai-completions",
        &p,
        &json!({"compat":{"thinkingFormat":"deepseek"}}),
        vec![user()],
        None,
        vec![],
        100,
    )
    .unwrap();
    assert_eq!(body["thinking"]["type"], "enabled");
    assert_eq!(body["reasoning_effort"], "high");
}

#[test]
fn foreign_reasoning_keeps_summary_and_drops_opaque_state() {
    let p = provider("openai", "openai-responses", "http://localhost/v1");
    let mut assistant = user();
    assistant.role = "assistant".into();
    assistant.provider_content = Some(json!({
        "api":"openai-responses","provider":"other-account","model":"fixture-model",
        "items":[
            {"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"kept"}],"encrypted_content":"opaque"},
            {"type":"reasoning","id":"rs_2","summary":[],"encrypted_content":"opaque-2"},
            {"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]}
        ]
    }));
    let body = messages::request(
        "openai-responses",
        &p,
        &Value::Null,
        vec![user(), assistant],
        Some("system"),
        vec![],
        4000,
    )
    .unwrap();
    let input = body["input"].as_array().unwrap();
    assert_eq!(input.len(), 3);
    assert!(input[1].get("encrypted_content").is_none());
    assert_eq!(input[1]["summary"][0]["text"], "kept");
    assert_eq!(input[2]["content"][0]["text"], "answer");
}

#[test]
fn legacy_reasoning_without_an_issuer_is_treated_as_foreign() {
    let p = provider("openai", "openai-responses", "http://localhost/v1");
    let mut assistant = user();
    assistant.role = "assistant".into();
    assistant.provider_content = Some(json!({
        "api":"openai-responses",
        "items":[
            {"type":"reasoning","id":"rs_old","summary":[{"type":"summary_text","text":"old"}],"encrypted_content":"opaque"},
            {"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]}
        ]
    }));
    let body = messages::request(
        "openai-responses",
        &p,
        &Value::Null,
        vec![assistant],
        None,
        vec![],
        100,
    )
    .unwrap();
    assert!(body["input"][0].get("encrypted_content").is_none());
    assert_eq!(body["input"][0]["summary"][0]["text"], "old");
}

#[test]
fn a_different_model_on_the_same_provider_is_foreign_too() {
    let p = provider("openai", "openai-responses", "http://localhost/v1");
    let mut assistant = user();
    assistant.role = "assistant".into();
    assistant.provider_content = Some(json!({
        "api":"openai-responses","provider":"test","model":"older-model",
        "items":[
            {"type":"reasoning","id":"rs_1","summary":[{"type":"summary_text","text":"kept"}],"encrypted_content":"opaque"},
            {"type":"message","role":"assistant","content":[{"type":"output_text","text":"answer"}]}
        ]
    }));
    let body = messages::request(
        "openai-responses",
        &p,
        &Value::Null,
        vec![assistant],
        None,
        vec![],
        100,
    )
    .unwrap();
    assert!(body["input"][0].get("encrypted_content").is_none());
    assert_eq!(body["input"][0]["summary"][0]["text"], "kept");
}

#[test]
fn foreign_anthropic_thinking_is_dropped_with_its_signature() {
    let p = provider("claude", "anthropic-messages", "http://localhost");
    let mut assistant = user();
    assistant.role = "assistant".into();
    assistant.provider_content = Some(json!({
        "api":"anthropic-messages","provider":"other","model":"fixture-model",
        "items":[
            {"type":"thinking","thinking":"private","signature":"sig"},
            {"type":"text","text":"done"}
        ]
    }));
    let body = messages::request(
        "anthropic-messages",
        &p,
        &Value::Null,
        vec![assistant],
        None,
        vec![],
        100,
    )
    .unwrap();
    let content = body["messages"][0]["content"].as_array().unwrap();
    assert_eq!(content.len(), 1);
    assert_eq!(content[0]["type"], "text");
}

#[test]
fn responses_replays_plain_thinking_into_an_empty_native_item() {
    let p = provider("openai", "openai-responses", "http://localhost/v1");
    let mut assistant = user();
    assistant.role = "assistant".into();
    assistant.reasoning_content = Some("prior plan".into());
    assistant.provider_content = Some(json!({
        "api":"openai-responses","provider":"test","model":"fixture-model",
        "items":[
            {"type":"reasoning","id":"rs_1","summary":[],"encrypted_content":"opaque"},
            {"type":"function_call","call_id":"call_1","name":"read","arguments":"{}"}
        ]
    }));
    let body = messages::request(
        "openai-responses",
        &p,
        &Value::Null,
        vec![assistant],
        None,
        vec![],
        100,
    )
    .unwrap();
    let input = body["input"].as_array().unwrap();
    let reasoning: Vec<_> = input.iter().filter(|item| item["type"] == "reasoning").collect();
    assert_eq!(reasoning.len(), 1);
    assert_eq!(reasoning[0]["summary"][0]["text"], "prior plan");
    assert_eq!(reasoning[0]["encrypted_content"], "opaque");
}

#[test]
fn responses_adds_plain_thinking_without_native_items() {
    let p = provider("openai", "openai-responses", "http://localhost/v1");
    let mut assistant = user();
    assistant.role = "assistant".into();
    assistant.reasoning_content = Some("prior plan".into());
    let body = messages::request(
        "openai-responses",
        &p,
        &Value::Null,
        vec![assistant],
        None,
        vec![],
        100,
    )
    .unwrap();
    let input = body["input"].as_array().unwrap();
    assert_eq!(input[0]["type"], "reasoning");
    assert_eq!(input[0]["summary"][0]["text"], "prior plan");
    assert_eq!(input[1]["role"], "assistant");
}

#[test]
fn chat_replays_reasoning_content() {
    let p = provider("deepseek", "openai-completions", "http://localhost");
    let mut assistant = user();
    assistant.role = "assistant".into();
    assistant.reasoning_content = Some("prior plan".into());
    let body = messages::request(
        "openai-completions",
        &p,
        &Value::Null,
        vec![assistant],
        None,
        vec![],
        100,
    )
    .unwrap();
    assert_eq!(body["messages"][0]["reasoning_content"], "prior plan");
}

#[test]
fn anthropic_stream_keeps_signatures_tools_and_cache_usage() {
    let mut s = events::State::default();
    let events = vec![
        json!({"type":"message_start","message":{"id":"msg_1","model":"claude","usage":{"input_tokens":2,"cache_read_input_tokens":3,"cache_creation_input_tokens":4}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"reason"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"signed"}}),
        json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"tool_1","name":"read","input":{}}}),
        json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"a\"}"}}),
        json!({"type":"content_block_stop","index":1}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":5}}),
        json!({"type":"message_stop"}),
    ];
    let out: Vec<_> = events
        .into_iter()
        .flat_map(|e| s.convert("anthropic-messages", e).unwrap())
        .collect();
    let last = out.last().unwrap();
    assert_eq!(last["provider_content"]["items"][0]["signature"], "signed");
    assert_eq!(last["provider_content"]["items"][1]["input"]["path"], "a");
    assert_eq!(last["usage"]["prompt_tokens"], 9);
    assert_eq!(last["usage"]["total_tokens"], 14);
    assert_eq!(last["choices"][0]["finish_reason"], "tool_calls");
    assert!(s.finished);
}

#[test]
fn gemini_keeps_thought_signatures_and_function_results() {
    let mut s = events::State::default();
    let out = s.convert("google-generative-ai",json!({"candidates":[{"content":{"parts":[{"thought":true,"text":"reason","thoughtSignature":"opaque"},{"functionCall":{"name":"read","args":{"path":"a"}},"thoughtSignature":"tool-signature"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":2,"candidatesTokenCount":3,"thoughtsTokenCount":4,"totalTokenCount":9}})).unwrap();
    let last = out.last().unwrap();
    assert_eq!(
        last["provider_content"]["items"][1]["thoughtSignature"],
        "tool-signature"
    );
    assert_eq!(last["usage"]["completion_tokens"], 7);
    assert_eq!(last["choices"][0]["finish_reason"], "tool_calls");
    assert!(last["provider_content"]["items"][1]["functionCall"]["id"]
        .as_str()
        .unwrap()
        .starts_with("call_"));
}

#[tokio::test]
async fn native_http_stream_uses_selected_endpoint_and_assembles_fragmented_calls() {
    type Requests = Arc<Mutex<Vec<(String, HeaderMap, Value)>>>;
    async fn handler(
        State(requests): State<Requests>,
        uri: Uri,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> String {
        requests
            .lock()
            .unwrap()
            .push((uri.to_string(), headers, body));
        [json!({"choices":[{"delta":{"content":"hello","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"read","arguments":"{\"p\":"}}]},"finish_reason":null}]}),json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"x\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":2,"completion_tokens":3,"total_tokens":5}})].into_iter().map(|v| format!("data: {v}\r\n\r\n")).collect::<String>() + "data: [DONE]\r\n\r\n"
    }
    let requests = Requests::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/custom/chat/completions", post(handler))
        .with_state(requests.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let p = provider(
        "deepseek",
        "openai-completions",
        &format!("http://{addr}/custom"),
    );
    let result = OpenAIClient::new()
        .send_message(&p, vec![user()], None, vec![], 1024)
        .await
        .unwrap();
    assert_eq!(
        result.choices[0].message.tool_calls.as_ref().unwrap()[0]
            .function
            .arguments,
        "{\"p\":\"x\"}"
    );
    let requests = requests.lock().unwrap();
    assert_eq!(requests[0].0, "/custom/chat/completions");
    assert_eq!(requests[0].1["authorization"], "Bearer fixture-only");
    assert_eq!(requests[0].2["max_tokens"], 1024);
    server.abort();
}

#[tokio::test]
async fn unsupported_protocol_fails_before_network_or_process_launch() {
    let p = provider("amazon-bedrock", "unknown-protocol", "http://127.0.0.1:1");
    let error = stream(&p, vec![user()], None, vec![], 100)
        .await
        .err()
        .unwrap()
        .to_string();
    assert!(error.contains("not implemented"));
}

#[test]
fn responses_preserves_output_and_reports_failure() {
    let mut state = events::State::default();
    let call = json!({"type":"function_call","id":"fc_1","call_id":"call_1","name":"read","arguments":"{}"});
    let out = state
        .convert(
            "openai-responses",
            json!({"type":"response.output_item.done","item":call}),
        )
        .unwrap();
    assert_eq!(
        out[0]["choices"][0]["delta"]["tool_calls"][0]["id"],
        "call_1"
    );
    let output = json!([{"type":"reasoning","encrypted_content":"encrypted"},call]);
    let out = state.convert("openai-responses",json!({"type":"response.completed","response":{"id":"resp_1","model":"gpt","status":"completed","output":output,"usage":{"input_tokens":3,"output_tokens":7,"total_tokens":10,"input_tokens_details":{"cached_tokens":2},"output_tokens_details":{"reasoning_tokens":4}}}})).unwrap();
    assert_eq!(out[0]["provider_content"]["items"], output);
    assert_eq!(
        out[0]["usage"]["completion_tokens_details"]["reasoning_tokens"],
        4
    );
    assert!(state
        .convert(
            "openai-responses",
            json!({"type":"response.failed","response":{"error":{"message":"fixture failure"}}})
        )
        .is_err());
}

#[tokio::test]
async fn fragmented_utf8_and_incomplete_stream_are_handled_without_data_loss() {
    async fn handler() -> axum::response::Response {
        let payload = format!(
            "data: {}\n\n",
            json!({"choices":[{"delta":{"content":"你好"},"finish_reason":null}]})
        );
        let bytes = payload
            .into_bytes()
            .into_iter()
            .map(|byte| Ok::<_, std::io::Error>(bytes::Bytes::from(vec![byte])));
        axum::response::Response::new(axum::body::Body::from_stream(futures::stream::iter(bytes)))
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route("/chat/completions", post(handler)),
        )
        .await
        .unwrap()
    });
    let p = provider("deepseek", "openai-completions", &format!("http://{addr}"));
    let mut response = stream(&p, vec![user()], None, vec![], 100).await.unwrap();
    assert!(response.next().await.unwrap().unwrap().contains("你好"));
    assert!(response
        .next()
        .await
        .unwrap()
        .unwrap_err()
        .to_string()
        .contains("before completion"));
    server.abort();
}

// Windows' local HTTP stack does not reliably surface peer body cancellation
// to the fixture; keep the Unix assertion where the observation is stable.
#[cfg(not(windows))]
#[tokio::test]
async fn dropping_stream_cancels_http_body() {
    use std::sync::atomic::{AtomicBool, Ordering};
    struct Dropped(Arc<AtomicBool>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    async fn handler(State(dropped): State<Arc<AtomicBool>>) -> axum::response::Response {
        let stream = async_stream::stream! {
            let _guard = Dropped(dropped);
            loop {
                yield Ok::<_,std::io::Error>(bytes::Bytes::from_static(b"data: {\"choices\":[{\"delta\":{\"content\":\"x\"},\"finish_reason\":null}]}\n\n"));
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        };
        axum::response::Response::new(axum::body::Body::from_stream(stream))
    }
    let dropped = Arc::new(AtomicBool::new(false));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = Router::new()
        .route("/chat/completions", post(handler))
        .with_state(dropped.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let p = provider("deepseek", "openai-completions", &format!("http://{addr}"));
    let mut response = stream(&p, vec![user()], None, vec![], 100).await.unwrap();
    response.next().await.unwrap().unwrap();
    drop(response);
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while !dropped.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("HTTP body was not cancelled");
    server.abort();
}

#[tokio::test]
async fn native_protocol_matrix_uses_correct_endpoints_headers_and_payloads() {
    type Captured = Arc<Mutex<Vec<(String, HeaderMap, Value)>>>;
    #[derive(Clone)]
    struct Fixture {
        requests: Captured,
        response: Vec<u8>,
    }
    async fn handler(
        State(state): State<Fixture>,
        uri: Uri,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Vec<u8> {
        state
            .requests
            .lock()
            .unwrap()
            .push((uri.to_string(), headers, body));
        state.response
    }
    let sse = |events: Vec<Value>| {
        events
            .into_iter()
            .map(|e| format!("data: {e}\n\n"))
            .collect::<String>()
            .into_bytes()
    };
    for (id, api, base, path) in [
        ("anthropic", "anthropic-messages", "", "/v1/messages"),
        ("openai", "openai-responses", "/v1", "/v1/responses"),
        (
            "google",
            "google-generative-ai",
            "/v1beta",
            "/v1beta/models/fixture-model:streamGenerateContent?alt=sse",
        ),
        (
            "google-vertex",
            "google-vertex",
            "/v1/publishers/google",
            "/v1/publishers/google/models/fixture-model:streamGenerateContent?alt=sse",
        ),
        (
            "azure-openai-responses",
            "azure-openai-responses",
            "/openai/v1",
            "/openai/v1/responses?api-version=v1",
        ),
        (
            "openai-codex",
            "openai-codex-responses",
            "/backend-api",
            "/backend-api/codex/responses",
        ),
        (
            "mistral",
            "mistral-conversations",
            "",
            "/v1/chat/completions",
        ),
        (
            "amazon-bedrock",
            "bedrock-converse-stream",
            "",
            "/model/fixture-model/converse-stream",
        ),
    ] {
        let response = match api {
            "anthropic-messages" => sse(vec![
                json!({"type":"message_start","message":{"id":"m","model":"fixture-model","usage":{"input_tokens":2}}}),
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":"native reply"}}),
                json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}),
                json!({"type":"message_stop"}),
            ]),
            "google-generative-ai" | "google-vertex" => sse(vec![
                json!({"candidates":[{"content":{"parts":[{"text":"native reply"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":2,"candidatesTokenCount":3,"totalTokenCount":5}}),
            ]),
            "bedrock-converse-stream" => [
                ("messageStart", json!({"role":"assistant"})),
                (
                    "contentBlockDelta",
                    json!({"contentBlockIndex":0,"delta":{"text":"native reply"}}),
                ),
                ("messageStop", json!({"stopReason":"end_turn"})),
                (
                    "metadata",
                    json!({"usage":{"inputTokens":2,"outputTokens":3,"totalTokens":5}}),
                ),
            ]
            .into_iter()
            .flat_map(|(kind, value)| bedrock::frame(kind, value))
            .collect(),
            "mistral-conversations" => sse(vec![
                json!({"choices":[{"delta":{"content":[{"type":"thinking","thinking":[{"type":"text","text":"reason"}]},{"type":"text","text":"native reply"}]},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":3,"total_tokens":5}}),
            ]),
            _ => sse(vec![
                json!({"type":"response.output_text.delta","delta":"native reply"}),
                json!({"type":"response.completed","response":{"id":"r","model":"fixture-model","status":"completed","output":[],"usage":{"input_tokens":2,"output_tokens":3,"total_tokens":5}}}),
            ]),
        };
        let requests = Captured::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = Router::new().fallback(post(handler)).with_state(Fixture {
            requests: requests.clone(),
            response,
        });
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut p = provider(id, api, &format!("http://{address}{base}"));
        if api == "openai-codex-responses" {
            p.provider.api_key = format!(
                "header.{}.signature",
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
                    json!({"https://api.openai.com/auth":{"chatgpt_account_id":"fixture-account"}})
                        .to_string()
                )
            );
        }
        let response = OpenAIClient::new()
            .send_message(&p, vec![user()], Some("system"), vec![], 4096)
            .await
            .unwrap_or_else(|e| panic!("{api}: {e:#}"));
        assert!(
            matches!(&response.choices[0].message.content,OpenAIContent::Text(s) if s=="native reply"),
            "{api}"
        );
        assert_eq!(response.usage.unwrap().total_tokens, 5, "{api}");
        let captured = requests.lock().unwrap();
        assert_eq!(captured[0].0, path, "{api}");
        let headers = &captured[0].1;
        let body = &captured[0].2;
        match api {
            "anthropic-messages" => {
                assert_eq!(headers["x-api-key"], "fixture-only");
                assert_eq!(body["max_tokens"], 4096);
                assert!(body["system"] == "system" || body["system"][0]["text"] == "system");
            }
            "google-generative-ai" | "google-vertex" => {
                assert_eq!(headers["x-goog-api-key"], "fixture-only");
                assert_eq!(body["generationConfig"]["maxOutputTokens"], 4096);
            }
            "azure-openai-responses" => {
                assert_eq!(headers["api-key"], "fixture-only");
                assert!(!headers.contains_key("authorization"));
            }
            "openai-codex-responses" => {
                assert_eq!(headers["chatgpt-account-id"], "fixture-account");
                assert!(body.get("max_output_tokens").is_none());
            }
            "bedrock-converse-stream" => {
                assert_eq!(headers["authorization"], "Bearer fixture-only");
                assert_eq!(body["inferenceConfig"]["maxTokens"], 4096);
            }
            _ => {
                assert_eq!(headers["authorization"], "Bearer fixture-only");
            }
        }
        server.abort();
    }
}

#[test]
fn opencode_go_models_take_their_base_url_and_protocol_from_the_catalog() {
    let entry = catalog_entry("opencode-go").expect("the opencode-go catalog entry");
    assert_eq!(entry["baseUrl"], "");
    let model = |id: &str| {
        entry["models"]
            .as_array()
            .unwrap()
            .iter()
            .find(|model| model["id"] == id)
            .unwrap_or_else(|| panic!("{id} is missing from the opencode-go catalog"))
    };
    let chat = model("deepseek-v4.1-flash");
    assert_eq!(chat["api"], "openai-completions");
    assert_eq!(
        format!("{}/chat/completions", resolved_base("", entry, chat)),
        "https://opencode.ai/inference/go/openai/v1/chat/completions"
    );
    let responses = model("gpt-6-luna");
    assert_eq!(responses["api"], "openai-responses");
    assert_eq!(
        resolved_base("", entry, responses),
        "https://opencode.ai/inference/go/openai/v1"
    );
    let messages = model("claude-haiku-5-5");
    assert_eq!(messages["api"], "anthropic-messages");
    assert_eq!(
        resolved_base("", entry, messages),
        "https://opencode.ai/inference/go/anthropic/v1"
    );
    let google = model("gemini-4-argon");
    assert_eq!(google["api"], "google-generative-ai");
    assert_eq!(
        resolved_base("", entry, google),
        "https://opencode.ai/inference/go/google/v1beta"
    );
    // An explicit gateway or mock address still wins.
    assert_eq!(
        resolved_base("http://127.0.0.1:9/v1", entry, chat),
        "http://127.0.0.1:9/v1"
    );
}

#[tokio::test]
async fn opencode_connection_test_accepts_optional_usage_and_rejects_invalid_usage() {
    async fn handler(
        State(payload): State<String>,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> String {
        // Connection tests have no conversation; routing falls back to the request ID.
        assert_eq!(headers["x-opencode-session"], headers["x-request-id"]);
        assert_eq!(body["stream"], true);
        payload
    }
    let valid = json!({"prompt_tokens":2,"completion_tokens":3,"total_tokens":5});
    for (name, usages, expected_total, fails) in [
        ("missing", vec![None], None, false),
        ("null_only", vec![Some(Value::Null)], None, false),
        (
            "null_before_usage",
            vec![Some(Value::Null), Some(valid.clone())],
            Some(5),
            false,
        ),
        (
            "null_after_usage",
            vec![Some(valid), Some(Value::Null), None],
            Some(5),
            false,
        ),
        ("invalid_non_null", vec![Some(json!("invalid"))], None, true),
    ] {
        let mut payload = String::new();
        for usage in usages {
            let mut chunk = json!({"choices":[{"delta":{"content":"hi"},"finish_reason":null}]});
            if let Some(usage) = usage {
                chunk["usage"] = usage;
            }
            payload.push_str(&format!("data: {chunk}\n\n"));
        }
        payload.push_str(
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new()
            .route("/chat/completions", post(handler))
            .with_state(payload);
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let p = provider(
            "opencode-go",
            "openai-completions",
            &format!("http://{addr}"),
        );
        let result = OpenAIClient::new()
            .send_message(&p, vec![user()], None, vec![], 100)
            .await;
        server.abort();
        if fails {
            assert!(
                result.is_err(),
                "{name}: invalid usage must remain an error"
            );
        } else {
            let response = result.unwrap_or_else(|error| panic!("{name}: {error:#}"));
            assert_eq!(
                response.usage.map(|u| u.total_tokens),
                expected_total,
                "{name}"
            );
            assert!(
                matches!(&response.choices[0].message.content, OpenAIContent::Text(text) if text.starts_with("hi")),
                "{name}"
            );
            assert_eq!(
                response.choices[0].finish_reason.as_deref(),
                Some("stop"),
                "{name}"
            );
        }
    }
}

#[path = "../../../tests/support/manual_clock.rs"]
mod manual_clock;

#[derive(Clone)]
enum ChatStep {
    /// A complete HTTP response written before the connection closes.
    Reply(Vec<u8>),
    /// Status line and headers only; the body never arrives.
    Hold(Vec<u8>),
    /// Headers plus raw SSE, then an interrupted chunked body (no zero chunk).
    Break(Vec<u8>),
}

fn http_bytes(status: &str, content_type: &str, body: &str) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body.as_bytes());
    out
}

fn sse(events: &[Value]) -> Vec<u8> {
    let body = events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();
    http_bytes("200 OK", "text/event-stream", &body)
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

fn header_value(raw: &[u8], name: &str) -> Option<String> {
    String::from_utf8_lossy(raw).lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name)
            .then(|| value.trim().to_owned())
    })
}

/// Real loopback server. Requests are answered in order; the last step repeats.
async fn chat_server(
    steps: Vec<ChatStep>,
    held: Option<tokio::sync::mpsc::UnboundedSender<usize>>,
) -> (
    String,
    Arc<Mutex<Vec<Vec<u8>>>>,
    Arc<AtomicUsize>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let requests: Arc<Mutex<Vec<Vec<u8>>>> = Default::default();
    let count = Arc::new(AtomicUsize::new(0));
    let captured = requests.clone();
    let total = count.clone();
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let index = total.fetch_add(1, Ordering::SeqCst);
            let captured = captured.clone();
            let held = held.clone();
            let steps = steps.clone();
            tokio::spawn(async move {
                let Ok(request) = read_request(&mut socket).await else {
                    return;
                };
                captured.lock().unwrap().push(request);
                match steps[index.min(steps.len() - 1)].clone() {
                    ChatStep::Reply(bytes) => {
                        let _ = socket.write_all(&bytes).await;
                        let _ = socket.shutdown().await;
                    }
                    ChatStep::Hold(bytes) => {
                        if socket.write_all(&bytes).await.is_ok() {
                            if let Some(held) = held {
                                let _ = held.send(index);
                            }
                            let _ = socket.flush().await;
                            // Keep the body open until the test's client timer fires.
                            tokio::time::sleep(Duration::from_secs(600)).await;
                        }
                    }
                    ChatStep::Break(bytes) => {
                        let _ = socket.write_all(&bytes).await;
                        let _ = socket.flush().await;
                        // Closing before the terminating zero chunk interrupts
                        // the body instead of ending it cleanly.
                        let _ = socket.shutdown().await;
                    }
                }
            });
        }
    });
    (base, requests, count, server)
}

/// Chunked SSE headers plus one data chunk, without a terminating zero chunk.
fn chunked_sse(body: &str) -> Vec<u8> {
    let mut out = b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n".to_vec();
    if !body.is_empty() {
        out.extend_from_slice(format!("{:x}\r\n", body.len()).as_bytes());
        out.extend_from_slice(body.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out
}

/// One attempt with a short injected client deadline. Returns the real POST
/// count, the normalized output, the failure and the per-attempt events.
async fn chat_attempt_outcome(
    steps: Vec<ChatStep>,
) -> (usize, String, Option<anyhow::Error>, Vec<Value>) {
    use crate::{native_engine::tool_loop::EngineEvent, performance::retry_events};
    let (base, _requests, count, server) = chat_server(steps, None).await;
    let p = provider("opencode-go", "openai-completions", &base);
    let options = crate::native_engine::request_options::RequestOptions::default();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(250))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    let (tx, mut events) = tokio::sync::mpsc::channel(64);
    let (text, error) = retry_events::scope(tx, async {
        let (mut stream, _) = super::stream_with_options_client(
            &p,
            vec![user()],
            None,
            vec![],
            128,
            None,
            None,
            &options,
            client,
        )
        .await
        .unwrap();
        let mut text = String::new();
        let mut error = None;
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(chunk) => text.push_str(&chunk),
                Err(failure) => {
                    error = Some(failure);
                    break;
                }
            }
        }
        (text, error)
    })
    .await;
    let mut attempt_events = Vec::new();
    while let Some(event) = events.recv().await {
        if let EngineEvent::RequestAttempt(event) = event {
            attempt_events.push(event);
        }
    }
    server.abort();
    (count.load(Ordering::SeqCst), text, error, attempt_events)
}

#[tokio::test(start_paused = true)]
async fn builtin_chat_retries_pre_content_timeout_under_one_budget() {
    use crate::{native_engine::tool_loop::EngineEvent, performance::retry_events};

    manual_clock::run(async {
        let (headers_tx, mut headers_rx) = tokio::sync::mpsc::unbounded_channel();
        let (base, requests, count, server) = chat_server(
            vec![
                ChatStep::Reply(http_bytes(
                    "503 Service Unavailable",
                    "application/json",
                    "{\"error\":{\"type\":\"overloaded_error\",\"message\":\"busy\"}}",
                )),
                ChatStep::Hold(
                    b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n"
                        .to_vec(),
                ),
                ChatStep::Reply(sse(&[
                    json!({"choices":[{"delta":{"content":"Recovered 中文"},"finish_reason":null}]}),
                    json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":2}}),
                ])),
            ],
            Some(headers_tx),
        )
        .await;
        let temp = tempfile::tempdir().unwrap();
        let options = crate::native_engine::request_options::RequestOptions {
            session_id: Some("builtin-chat-fixture".into()),
            purpose: "main".into(),
            recording: Some(crate::native_engine::request_recording::RecordingContext {
                root: temp.path().to_path_buf(),
                parent_session_id: None,
            }),
            ..Default::default()
        };
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(250))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let p = provider("opencode-go", "openai-completions", &base);
        let (tx, mut events) = tokio::sync::mpsc::channel(64);
        let pending = retry_events::scope(tx, async {
            let (mut stream, counter) = super::stream_with_options_client(
                &p,
                vec![user()],
                None,
                vec![],
                128,
                None,
                None,
                &options,
                client,
            )
            .await
            .unwrap();
            let mut text = String::new();
            let mut error = None;
            while let Some(chunk) = stream.next().await {
                match chunk {
                    Ok(chunk) => text.push_str(&chunk),
                    Err(failure) => {
                        error = Some(failure);
                        break;
                    }
                }
            }
            (counter, text, error)
        });
        tokio::pin!(pending);
        let mut waits = Vec::new();
        let outcome;
        loop {
            tokio::select! {
                biased;
                done = &mut pending => {
                    outcome = done;
                    break;
                }
                Some(_) = headers_rx.recv() => {
                    // Let the runtime receive the flushed headers and start the
                    // body read before the injected client timer expires.
                    for _ in 0..256 {
                        tokio::task::yield_now().await;
                    }
                    tokio::time::advance(Duration::from_millis(300)).await;
                }
                event = events.recv() => {
                    if let Some(EngineEvent::RequestAttempt(event)) = event {
                        if event["outcome"] == "retry_wait" {
                            let wait = event["waitMs"].as_u64().unwrap();
                            waits.push(wait);
                            tokio::time::advance(Duration::from_millis(wait + 1)).await;
                        }
                    }
                }
            }
        }
        let (counter, text, error) = outcome;
        server.abort();
        assert!(error.is_none(), "{error:?}");
        assert_eq!(
            count.load(Ordering::SeqCst),
            3,
            "{}",
            String::from_utf8_lossy(&requests.lock().unwrap()[0])
        );
        assert_eq!(waits, vec![2000, 4000], "both reconnects waited");
        assert_eq!(text.matches("Recovered 中文").count(), 1, "{text}");
        assert_eq!(counter.attempts(), Some(3));
        assert_eq!(counter.status(), Some(200));

        let captured = requests.lock().unwrap();
        let request_id = header_value(&captured[0], "x-request-id").unwrap();
        let session = header_value(&captured[0], "x-opencode-session").unwrap();
        for (index, request) in captured.iter().enumerate() {
            assert_eq!(header_value(request, "x-request-id").as_deref(), Some(request_id.as_str()));
            assert_eq!(header_value(request, "x-opencode-session").as_deref(), Some(session.as_str()));
            assert_eq!(header_value(request, "x-dcl-retry-count"), Some(index.to_string()));
        }
        drop(captured);

        let records = temp.path().join("diagnostics/request-records/builtin-chat-fixture");
        let mut metas: Vec<std::path::PathBuf> = std::fs::read_dir(&records)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.to_string_lossy().ends_with(".meta.json"))
            .collect();
        metas.sort();
        assert_eq!(metas.len(), 3, "three physical attempts were recorded");
        let completed = metas
            .iter()
            .filter(|path| {
                let meta: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
                meta["stream_completed"] == true
            })
            .count();
        assert_eq!(completed, 1, "exactly one response body completed");
    })
    .await;
}

const EMPTY_PRELUDE: &str =
    "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"},\"finish_reason\":null}]}\n\n";

#[tokio::test(start_paused = true)]
async fn builtin_chat_pre_content_timeout_exhausts_and_can_be_cancelled() {
    use crate::{
        native_engine::tool_loop::EngineEvent,
        performance::{find_failure, retry_events},
    };

    manual_clock::run(async {
        let empty = http_bytes("200 OK", "text/event-stream", EMPTY_PRELUDE);
        let options = crate::native_engine::request_options::RequestOptions::default();
        for run in 0..2 {
            let (base, _requests, count, server) =
                chat_server(vec![ChatStep::Reply(empty.clone())], None).await;
            let p = provider("opencode-go", "openai-completions", &base);
            let (tx, mut events) = tokio::sync::mpsc::channel(64);
            if run == 0 {
                let pending = retry_events::scope(tx, async {
                    let (mut stream, counter) = super::stream_with_options_counted(
                        &p,
                        vec![user()],
                        None,
                        vec![],
                        128,
                        None,
                        None,
                        &options,
                    )
                    .await
                    .unwrap();
                    let mut text = String::new();
                    let mut error = None;
                    while let Some(chunk) = stream.next().await {
                        match chunk {
                            Ok(chunk) => text.push_str(&chunk),
                            Err(failure) => {
                                error = Some(failure);
                                break;
                            }
                        }
                    }
                    (counter, text, error)
                });
                tokio::pin!(pending);
                let mut waits = Vec::new();
                let outcome;
                loop {
                    tokio::select! {
                        biased;
                        done = &mut pending => {
                            outcome = done;
                            break;
                        }
                        event = events.recv() => {
                            if let Some(EngineEvent::RequestAttempt(event)) = event {
                                if event["outcome"] == "retry_wait" {
                                    let wait = event["waitMs"].as_u64().unwrap();
                                    waits.push(wait);
                                    tokio::time::advance(Duration::from_millis(wait)).await;
                                }
                            }
                        }
                    }
                }
                let (counter, text, error) = outcome;
                server.abort();
                assert_eq!(count.load(Ordering::SeqCst), 8);
                assert_eq!(
                    waits,
                    vec![2000, 4000, 8000, 16000, 32000, 64000, 128000],
                    "seven production waits"
                );
                assert_eq!(
                    text.matches("\"role\":\"assistant\"").count(),
                    1,
                    "the exhausted prelude is exposed once: {text}"
                );
                assert_eq!(counter.attempts(), Some(8));
                let error = error.expect("the exhausted stream reports a typed failure");
                let failure = find_failure(&error).expect("typed failure retained");
                assert_eq!(failure.status(), Some(200));
                assert_eq!(failure.attempts(), Some(8));
                assert_eq!(failure.retries(), Some(7));
                assert_eq!(failure.content_started(), Some(false));
            } else {
                let mut pending = Box::pin(retry_events::scope(tx, async {
                    let (mut stream, _) = super::stream_with_options_counted(
                        &p,
                        vec![user()],
                        None,
                        vec![],
                        128,
                        None,
                        None,
                        &options,
                    )
                    .await
                    .unwrap();
                    stream.next().await
                }));
                loop {
                    tokio::select! {
                        biased;
                        result = &mut pending => panic!("stream ended before a retry wait: {result:?}"),
                        event = events.recv() => {
                            if let Some(EngineEvent::RequestAttempt(event)) = event {
                                if event["outcome"] == "retry_wait" {
                                    break;
                                }
                            }
                        }
                    }
                }
                drop(pending);
                tokio::time::advance(Duration::from_secs(3)).await;
                server.abort();
                assert_eq!(count.load(Ordering::SeqCst), 1, "a cancelled wait sent another request");
            }
        }
    })
    .await;
}

async fn chat_case(steps: Vec<ChatStep>) -> (usize, String, Option<anyhow::Error>) {
    let (base, _requests, count, server) = chat_server(steps, None).await;
    let p = provider("opencode-go", "openai-completions", &base);
    let options = crate::native_engine::request_options::RequestOptions::default();
    let mut stream =
        super::stream_with_options(&p, vec![user()], None, vec![], 128, None, None, &options)
            .await
            .unwrap();
    let mut text = String::new();
    let mut error = None;
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(chunk) => text.push_str(&chunk),
            Err(failure) => {
                error = Some(failure);
                break;
            }
        }
    }
    let requests = count.load(Ordering::SeqCst);
    server.abort();
    (requests, text, error)
}

#[tokio::test]
async fn builtin_chat_never_replays_content_or_an_incomplete_sse_frame() {
    let content = json!({"choices":[{"delta":{"content":"done"},"finish_reason":null}]});
    let stop = json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}});
    let cases = [
        (
            "reasoning",
            sse(&[
                json!({"choices":[{"delta":{"reasoning_content":"thinking"},"finish_reason":null}]}),
            ]),
            "thinking",
        ),
        (
            "partial tool arguments",
            sse(&[
                json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call","type":"function","function":{"name":"Bash","arguments":"{\"command\":"}}]},"finish_reason":null}]}),
            ]),
            "tool_calls",
        ),
        (
            "unknown data",
            sse(&[json!({"unexpected":true})]),
            "unexpected",
        ),
        (
            "truncated raw frame",
            http_bytes(
                "200 OK",
                "text/event-stream",
                "data: {\"choices\":[{\"delta\":{\"content\":\"partial",
            ),
            "inside an SSE frame",
        ),
        (
            "provider error",
            sse(&[json!({"error":{"type":"invalid_request_error","message":"fixture rejected"}})]),
            // Only the known identifier survives; the provider text does not.
            "invalid_request_error",
        ),
    ];
    for (name, reply, expected) in cases {
        let (requests, text, error) = chat_case(vec![ChatStep::Reply(reply)]).await;
        assert_eq!(requests, 1, "{name}: content must not be replayed");
        let error = error.unwrap_or_else(|| panic!("{name}: expected a failure"));
        let diagnostic = format!("{error:#}");
        assert!(
            text.contains(expected) || diagnostic.contains(expected),
            "{name}: missing {expected} in text={text} diagnostic={diagnostic}"
        );
    }
    let (requests, text, error) =
        chat_case(vec![ChatStep::Reply(sse(&[content.clone(), stop.clone()]))]).await;
    assert_eq!(requests, 1);
    assert!(error.is_none(), "{error:?}");
    assert!(text.contains("done") && text.contains("[DONE]"), "{text}");

    // Control: a clean role-only EOF is a wholly empty prelude, not content.
    let role_only =
        sse(&[json!({"choices":[{"delta":{"role":"assistant"},"finish_reason":null}]})]);
    let (requests, text, error) = chat_case(vec![
        ChatStep::Reply(role_only),
        ChatStep::Reply(sse(&[
            json!({"choices":[{"delta":{"content":"Recovered"},"finish_reason":null}]}),
            stop,
        ])),
    ])
    .await;
    assert!(error.is_none(), "{error:?}");
    assert_eq!(requests, 2, "the empty prelude is reopened once");
    assert_eq!(text.matches("Recovered").count(), 1, "{text}");
}

#[tokio::test]
async fn builtin_chat_body_error_and_timeout_after_raw_replay_barriers_send_one_post() {
    use crate::performance::{find_failure, ErrorClass, RequestStage};
    let replacement = sse(&[
        json!({"choices":[{"delta":{"content":"REPLAYED"},"finish_reason":null}]}),
        json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}),
    ]);
    let barriers = [
        (
            "unfinished raw frame",
            "data: {\"choices\":[{\"delta\":{\"content\":\"truncated",
        ),
        ("standalone done", "data: [DONE]\n\n"),
        (
            "opaque content part",
            "data: {\"choices\":[{\"delta\":{\"content\":[{\"type\":\"fixture-extension\",\"metadata\":{\"state\":\"present\"}}]},\"finish_reason\":null}]}\n\n",
        ),
    ];
    for (name, raw) in barriers {
        for timeout in [false, true] {
            let step = if timeout {
                ChatStep::Hold(chunked_sse(raw))
            } else {
                ChatStep::Break(chunked_sse(raw))
            };
            let (posts, text, error, attempts) =
                chat_attempt_outcome(vec![step, ChatStep::Reply(replacement.clone())]).await;
            let label = format!("{name}/{}", if timeout { "timeout" } else { "body error" });
            assert_eq!(posts, 1, "{label}: an unsafe replay was sent");
            assert!(
                !text.contains("REPLAYED"),
                "{label}: replacement text: {text}"
            );
            assert!(
                !text.contains("[DONE]"),
                "{label}: no completion may be reported: {text}"
            );
            assert!(
                attempts
                    .iter()
                    .all(|event| event["outcome"] != "retry_wait"),
                "{label}: a reconnect was scheduled: {attempts:?}"
            );
            let error = error.unwrap_or_else(|| panic!("{label}: expected a typed failure"));
            let failure = find_failure(&error)
                .unwrap_or_else(|| panic!("{label}: typed failure retained: {error:#}"));
            assert_eq!(failure.stage(), RequestStage::Stream, "{label}");
            assert_eq!(failure.status(), Some(200), "{label}");
            assert_eq!(failure.attempts(), Some(1), "{label}");
            assert_eq!(failure.retries(), Some(0), "{label}");
            if timeout {
                assert_eq!(
                    failure.class(),
                    ErrorClass::Transient,
                    "{label}: {}",
                    failure.cause()
                );
                assert!(
                    failure.cause().contains("timed out"),
                    "{label}: {}",
                    failure.cause()
                );
            }
        }
    }

    // Control: a clean EOF half-frame stays a single, final POST.
    let (posts, text, error, _) = chat_attempt_outcome(vec![
        ChatStep::Reply(http_bytes(
            "200 OK",
            "text/event-stream",
            "data: {\"choices\":[{\"delta\":{\"content\":\"truncated",
        )),
        ChatStep::Reply(replacement.clone()),
    ])
    .await;
    assert_eq!(posts, 1, "a clean half-frame was replayed: {text}");
    assert!(!text.contains("REPLAYED"), "{text}");
    let clean_error = error.expect("a clean half-frame fails");
    let failure = find_failure(&clean_error).expect("typed failure");
    assert_eq!(failure.stage(), RequestStage::Stream);
    assert_eq!(failure.attempts(), Some(1));

    // Control: a complete stream still answers exactly once.
    let (posts, text, error, _) = chat_attempt_outcome(vec![ChatStep::Reply(replacement)]).await;
    assert_eq!(posts, 1);
    assert!(error.is_none(), "{error:?}");
    assert_eq!(text.matches("REPLAYED").count(), 1, "{text}");
}

/// The standalone built-in Chat entry must reject a malformed orchestration
/// file before it opens a connection. The child process is a fresh binary with
/// an uninitialized global configuration, so a test singleton cannot satisfy it.
#[tokio::test]
async fn builtin_chat_invalid_orchestration_fails_before_open() {
    const CHILD_BASE: &str = "DCL_CHAT_CONFIG_CHILD_BASE";
    if let Ok(base) = std::env::var(CHILD_BASE) {
        let p = provider("opencode-go", "openai-completions", &base);
        let result = super::stream_with_options(
            &p,
            vec![user()],
            None,
            vec![],
            128,
            None,
            None,
            &Default::default(),
        )
        .await;
        assert!(
            result.is_err(),
            "malformed orchestration must fail before any request"
        );
        let text = format!("{:#}", result.err().unwrap()).to_ascii_lowercase();
        assert!(
            text.contains("toml") || text.contains("orchestration"),
            "the configuration error must be returned: {text}"
        );
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(temp.path().join("config")).unwrap();
    crate::data_layout::write_config_file(
        temp.path().join("config/orchestration.toml"),
        "this is not toml [[[",
    )
    .unwrap();
    let (base, _requests, count, server) = chat_server(
        vec![ChatStep::Reply(sse(&[
            json!({"choices":[{"delta":{"content":"unexpected"},"finish_reason":"stop"}]}),
        ]))],
        None,
    )
    .await;
    let child = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "native_engine::provider_runtime::tests::builtin_chat_invalid_orchestration_fails_before_open",
            "--nocapture",
        ])
        .current_dir(temp.path())
        .env(CHILD_BASE, &base)
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
    let posts = count.load(Ordering::SeqCst);
    server.abort();
    assert!(
        output.contains("1 passed"),
        "the child did not run the exact test: {output}"
    );
    assert!(
        child.status.success(),
        "the child did not report the configuration error: {output}"
    );
    assert_eq!(posts, 0, "the transport opened a request: {output}");
}

/// A send retry and a pre-content body retry share one cap: with
/// `max_retries = 1`, a 503 followed by a role-only EOF ends after two POSTs.
#[tokio::test]
async fn builtin_chat_mixed_exhaustion_ends_at_the_shared_cap() {
    const CHILD: &str = "DCL_CHAT_MIXED_CAP_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("config")).unwrap();
        crate::data_layout::write_config_file(
            temp.path().join("config/orchestration.toml"),
            "[tool_loop]\n[tools]\n[memory]\nenabled = false\n[bridge]\n[http]\n\
             request_timeout_secs = 30\nmax_retries = 1\nretry_base_backoff_ms = 20\n\
             [logging]\n[telemetry]\n",
        )
        .unwrap();
        let child = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "native_engine::provider_runtime::tests::builtin_chat_mixed_exhaustion_ends_at_the_shared_cap",
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
        assert!(
            output.contains("1 passed") && child.status.success(),
            "the mixed-exhaustion control failed: {output}"
        );
        return;
    }

    use crate::{
        native_engine::tool_loop::EngineEvent,
        performance::{find_failure, retry_events, RequestStage},
    };
    let (base, _requests, count, server) = chat_server(
        vec![
            ChatStep::Reply(http_bytes(
                "503 Service Unavailable",
                "application/json",
                "{\"error\":{\"type\":\"overloaded_error\",\"message\":\"busy\"}}",
            )),
            ChatStep::Reply(http_bytes("200 OK", "text/event-stream", EMPTY_PRELUDE)),
            ChatStep::Reply(sse(&[
                json!({"choices":[{"delta":{"content":"REPLAYED"},"finish_reason":"stop"}]}),
            ])),
        ],
        None,
    )
    .await;
    let p = provider("opencode-go", "openai-completions", &base);
    let options = crate::native_engine::request_options::RequestOptions::default();
    let (tx, mut events) = tokio::sync::mpsc::channel(64);
    let (text, error) = retry_events::scope(tx, async {
        let (mut stream, _) = super::stream_with_options_counted(
            &p,
            vec![user()],
            None,
            vec![],
            128,
            None,
            None,
            &options,
        )
        .await
        .unwrap();
        let mut text = String::new();
        let mut error = None;
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(chunk) => text.push_str(&chunk),
                Err(failure) => {
                    error = Some(failure);
                    break;
                }
            }
        }
        (text, error)
    })
    .await;
    let mut waits = Vec::new();
    while let Some(event) = events.recv().await {
        if let EngineEvent::RequestAttempt(event) = event {
            if event["outcome"] == "retry_wait" {
                waits.push(event["waitMs"].as_u64().unwrap());
            }
        }
    }
    server.abort();
    assert_eq!(
        count.load(Ordering::SeqCst),
        2,
        "the shared cap was exceeded: {text}"
    );
    assert_eq!(waits, vec![20], "exactly one retry wait");
    assert!(!text.contains("REPLAYED"), "{text}");
    let capped = error.expect("the cap is a typed failure");
    let failure = find_failure(&capped).expect("typed failure");
    assert_eq!(failure.stage(), RequestStage::Stream);
    assert_eq!(failure.status(), Some(200));
    assert_eq!(failure.attempts(), Some(2));
    assert_eq!(failure.retries(), Some(1));
}

#[derive(Clone, Copy, Debug)]
enum SingleTransport {
    Builtin,
    Direct,
}

fn direct_provider(url: &str) -> ResolvedProvider {
    let provider: Provider = serde_json::from_value(json!({
        "id":"direct","name":"direct","base_url":url,"api_key":"fixture-only",
        "api_format":"openai","enabled":true,
        "models":[{"id":"fixture-model","name":"fixture","enabled":true,"max_tokens":8192,
                   "supports_vision":false,"supports_web_search":false}]
    }))
    .unwrap();
    ResolvedProvider {
        model: provider.models[0].clone(),
        provider,
    }
}

/// Answers every request with a 307 pointing at `location`.
async fn redirect_server(
    location: String,
) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let count = Arc::new(AtomicUsize::new(0));
    let total = count.clone();
    let server = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            total.fetch_add(1, Ordering::SeqCst);
            let location = location.clone();
            tokio::spawn(async move {
                if read_request(&mut socket).await.is_err() {
                    return;
                }
                let response = format!(
                    "HTTP/1.1 307 Temporary Redirect\r\nlocation: {location}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (base, count, server)
}

async fn open_single(
    transport: SingleTransport,
    base: &str,
    options: &crate::native_engine::request_options::RequestOptions,
) -> Result<(super::ProviderStream, crate::performance::AttemptCounter)> {
    match transport {
        SingleTransport::Builtin => {
            let p = provider("opencode-go", "openai-completions", base);
            super::stream_with_options_counted_budget(
                &p,
                vec![user()],
                None,
                vec![],
                128,
                None,
                None,
                options,
                crate::performance::AttemptBudget::single_attempt(),
            )
            .await
        }
        SingleTransport::Direct => {
            let p = direct_provider(base);
            OpenAIClient::new()
                .with_request_options(options.clone())
                .send_message_stream_counted_budget(
                    &p,
                    vec![user()],
                    None,
                    vec![],
                    128,
                    None,
                    crate::performance::AttemptBudget::single_attempt(),
                )
                .await
        }
    }
}

/// Consume one single-attempt run and count its `retry_wait` events.
async fn single_run(
    transport: SingleTransport,
    base: &str,
    options: &crate::native_engine::request_options::RequestOptions,
) -> (
    String,
    Option<anyhow::Error>,
    Option<u32>,
    Option<u16>,
    usize,
) {
    use crate::native_engine::tool_loop::EngineEvent;
    use crate::performance::retry_events;
    let (tx, mut events) = tokio::sync::mpsc::channel(64);
    let (text, error, attempts, status) = retry_events::scope(tx, async {
        match open_single(transport, base, options).await {
            Err(error) => (String::new(), Some(error), None, None),
            Ok((mut stream, counter)) => {
                let mut text = String::new();
                let mut error = None;
                while let Some(chunk) = stream.next().await {
                    match chunk {
                        Ok(chunk) => text.push_str(&chunk),
                        Err(failure) => {
                            error = Some(failure);
                            break;
                        }
                    }
                }
                (text, error, counter.attempts(), counter.status())
            }
        }
    })
    .await;
    let mut retry_waits = 0;
    while let Some(event) = events.recv().await {
        if let EngineEvent::RequestAttempt(event) = event {
            if event["outcome"] == "retry_wait" {
                retry_waits += 1;
            }
        }
    }
    (text, error, attempts, status, retry_waits)
}

#[tokio::test]
async fn single_attempt_chat_stream_never_spends_a_second_post() {
    const CHILD: &str = "DCL_SINGLE_ATTEMPT_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("config")).unwrap();
        crate::data_layout::write_config_file(
            temp.path().join("config/orchestration.toml"),
            "[tool_loop]\n[tools]\n[memory]\nenabled = false\n[bridge]\n[http]\n\
             request_timeout_secs = 1\nmax_retries = 2\nretry_base_backoff_ms = 5\n\
             [logging]\n[telemetry]\n",
        )
        .unwrap();
        let child = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "native_engine::provider_runtime::tests::single_attempt_chat_stream_never_spends_a_second_post",
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
        assert!(
            output.contains("1 passed") && child.status.success(),
            "the single-attempt child failed: {output}"
        );
        return;
    }

    use crate::performance::{find_failure, AttemptBudget, RequestStage};
    assert_eq!(
        AttemptBudget::configured().max(),
        3,
        "the fixture keeps a normal budget of three"
    );
    let options = crate::native_engine::request_options::RequestOptions::default();
    for transport in [SingleTransport::Builtin, SingleTransport::Direct] {
        // Retryable status: one POST, the failure keeps the real status.
        let (base, _requests, count, server) = chat_server(
            vec![ChatStep::Reply(http_bytes(
                "503 Service Unavailable",
                "application/json",
                "{\"error\":{\"type\":\"overloaded_error\",\"message\":\"busy\"}}",
            ))],
            None,
        )
        .await;
        let (_, error, _, _, waits) = single_run(transport, &base, &options).await;
        server.abort();
        assert_eq!(count.load(Ordering::SeqCst), 1, "{transport:?} 503");
        assert_eq!(waits, 0, "{transport:?} 503 retried");
        let failure = find_failure(error.as_ref().expect("503 fails")).unwrap();
        assert_eq!(failure.stage(), RequestStage::Response, "{transport:?}");
        assert_eq!(failure.status(), Some(503));
        assert_eq!(failure.attempts(), Some(1));

        // Header wait: one POST, send stage, no typed body-timeout fact.
        let (base, _requests, count, server) =
            chat_server(vec![ChatStep::Hold(Vec::new())], None).await;
        let (_, error, _, _, waits) = single_run(transport, &base, &options).await;
        server.abort();
        assert_eq!(count.load(Ordering::SeqCst), 1, "{transport:?} headers");
        assert_eq!(waits, 0, "{transport:?} headers retried");
        let failure = find_failure(error.as_ref().expect("header wait fails")).unwrap();
        assert_eq!(failure.stage(), RequestStage::Send, "{transport:?}");
        assert_eq!(failure.status(), None);
        assert_eq!(failure.attempts(), Some(1));
        assert!(!failure.is_stream_body_timeout());

        // Flushed empty prelude then a stalled body: one POST, no reopen.
        let (base, _requests, count, server) =
            chat_server(vec![ChatStep::Hold(chunked_sse(EMPTY_PRELUDE))], None).await;
        let (_, error, attempts, status, waits) = single_run(transport, &base, &options).await;
        server.abort();
        assert_eq!(count.load(Ordering::SeqCst), 1, "{transport:?} body");
        assert_eq!(waits, 0, "{transport:?} body retried");
        let failure = find_failure(error.as_ref().expect("body timeout fails")).unwrap();
        assert_eq!(failure.stage(), RequestStage::Stream, "{transport:?}");
        assert_eq!(failure.status(), Some(200));
        assert_eq!(failure.attempts(), Some(1));
        assert!(failure.is_stream_body_timeout());
        assert_eq!(attempts, Some(1));
        assert_eq!(status, Some(200));

        // A 307 must not reach its target: the replacement client does not follow.
        let (target, _target_requests, target_count, target_server) = chat_server(
            vec![ChatStep::Reply(sse(&[json!({
                "choices":[{"delta":{"content":"REPLACED"},"finish_reason":"stop"}]
            })]))],
            None,
        )
        .await;
        let (base, count, server) = redirect_server(format!("{target}/chat/completions")).await;
        let (_, error, _, _, waits) = single_run(transport, &base, &options).await;
        server.abort();
        target_server.abort();
        assert_eq!(count.load(Ordering::SeqCst), 1, "{transport:?} redirect");
        assert_eq!(
            target_count.load(Ordering::SeqCst),
            0,
            "{transport:?} followed the redirect"
        );
        assert_eq!(waits, 0, "{transport:?} redirect retried");
        let failure = find_failure(error.as_ref().expect("307 fails")).unwrap();
        assert_eq!(failure.stage(), RequestStage::Response, "{transport:?}");
        assert_eq!(failure.status(), Some(307));

        // Healthy completion: exact output, one POST, usual auth, identity and
        // recording paths.
        let (base, requests, count, server) = chat_server(
            vec![ChatStep::Reply(sse(&[
                json!({"choices":[{"delta":{"content":"ok"},"finish_reason":null}]}),
                json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}),
            ]))],
            None,
        )
        .await;
        let temp = tempfile::tempdir().unwrap();
        let session = format!("single-attempt-{transport:?}");
        let recording = crate::native_engine::request_options::RequestOptions {
            session_id: Some(session.clone()),
            purpose: "main".into(),
            recording: Some(crate::native_engine::request_recording::RecordingContext {
                root: temp.path().to_path_buf(),
                parent_session_id: None,
            }),
            ..Default::default()
        };
        let (text, error, attempts, status, waits) = single_run(transport, &base, &recording).await;
        server.abort();
        assert!(error.is_none(), "{transport:?}: {error:?}");
        assert_eq!(text.matches("\"content\":\"ok\"").count(), 1, "{text}");
        assert_eq!(
            text.matches("\"finish_reason\":\"stop\"").count(),
            1,
            "{text}"
        );
        assert_eq!(count.load(Ordering::SeqCst), 1, "{transport:?} healthy");
        assert_eq!(attempts, Some(1));
        assert_eq!(status, Some(200));
        assert_eq!(waits, 0);
        let captured = requests.lock().unwrap();
        assert_eq!(
            header_value(&captured[0], "authorization").as_deref(),
            Some("Bearer fixture-only"),
            "{transport:?}"
        );
        assert!(
            header_value(&captured[0], "x-request-id").is_some(),
            "{transport:?} identity"
        );
        assert_eq!(
            header_value(&captured[0], "x-dcl-retry-count").as_deref(),
            Some("0"),
            "{transport:?}"
        );
        drop(captured);
        let records = temp
            .path()
            .join("diagnostics/request-records")
            .join(&session);
        let metas = std::fs::read_dir(records)
            .map(|dir| {
                dir.filter_map(Result::ok)
                    .filter(|entry| entry.path().to_string_lossy().ends_with(".meta.json"))
                    .count()
            })
            .unwrap_or(0);
        assert_eq!(metas, 1, "{transport:?} recording");
    }
}

/// Both default counted OpenAI routes must reject a malformed orchestration
/// file before the retry budget caches built-in defaults. Each child is a fresh
/// binary with an uninitialized singleton, so only the entry itself can fail.
#[tokio::test]
async fn openai_counted_stream_invalid_orchestration_fails_before_open() {
    const CHILD: &str = "DCL_COUNTED_CONFIG_CHILD";
    const BASE: &str = "DCL_COUNTED_CONFIG_BASE";
    if let (Ok(route), Ok(base)) = (std::env::var(CHILD), std::env::var(BASE)) {
        let p = if route == "builtin" {
            provider("opencode-go", "openai-completions", &base)
        } else {
            direct_provider(&base)
        };
        let result = OpenAIClient::new()
            .send_message_stream_counted(&p, vec![user()], None, vec![], 128, None)
            .await;
        assert!(
            result.is_err(),
            "{route}: malformed orchestration must fail before any request"
        );
        let text = format!("{:#}", result.err().unwrap()).to_ascii_lowercase();
        assert!(
            text.contains("toml") || text.contains("orchestration"),
            "{route}: the configuration error must be returned: {text}"
        );
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(temp.path().join("config")).unwrap();
    crate::data_layout::write_config_file(
        temp.path().join("config/orchestration.toml"),
        "this is not toml [[[",
    )
    .unwrap();
    // A valid response, so a POST that incorrectly reaches the wire is answered.
    let (base, _requests, count, server) = chat_server(
        vec![ChatStep::Reply(sse(&[json!({
            "choices":[{"delta":{"content":"unexpected"},"finish_reason":"stop"}]
        })]))],
        None,
    )
    .await;
    for route in ["builtin", "direct"] {
        let child = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "native_engine::provider_runtime::tests::openai_counted_stream_invalid_orchestration_fails_before_open",
                "--nocapture",
            ])
            .current_dir(temp.path())
            .env(CHILD, route)
            .env(BASE, &base)
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
        assert!(
            output.contains("1 passed"),
            "{route}: the child did not run the exact test: {output}"
        );
        assert!(
            child.status.success(),
            "{route}: the child did not report the configuration error: {output}"
        );
        assert_eq!(
            count.load(Ordering::SeqCst),
            0,
            "{route}: the transport opened a request: {output}"
        );
    }
    server.abort();
}

#[tokio::test]
async fn responses_requests_send_a_stable_hashed_prompt_cache_key() {
    use crate::native_engine::request_identity::TurnIdentity;

    type Captured = Arc<Mutex<Vec<Value>>>;
    async fn handler(
        State(requests): State<Captured>,
        Json(body): Json<Value>,
    ) -> (StatusCode, Json<Value>) {
        requests.lock().unwrap().push(body);
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":{"message":"fixture rejection"}})),
        )
    }
    async fn send(address: std::net::SocketAddr, api: &str) {
        let p = provider("openai", api, &format!("http://{address}/v1"));
        // Every body is captured before the fixture rejects the request.
        let _ = OpenAIClient::new()
            .send_message(&p, vec![user()], None, vec![], 1024)
            .await;
    }

    let requests = Captured::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new()
        .fallback(post(handler))
        .with_state(requests.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    TurnIdentity::new(Some("session-a".into()), None)
        .scope(async {
            send(address, "openai-responses").await;
            send(address, "openai-responses").await;
        })
        .await;
    TurnIdentity::new(Some("session-b".into()), None)
        .scope(send(address, "openai-responses"))
        .await;
    send(address, "openai-responses").await;
    TurnIdentity::new(Some("session-c".into()), None)
        .scope(async {
            send(address, "openai-completions").await;
            send(address, "anthropic-messages").await;
        })
        .await;

    let bodies = requests.lock().unwrap();
    assert_eq!(bodies.len(), 6);
    let key = |body: &Value| body["prompt_cache_key"].as_str().map(str::to_owned);
    let first = key(&bodies[0]).expect("a Responses request carries a cache key");
    assert_eq!(first, key(&bodies[1]).expect("same-session key"));
    assert_eq!(first.len(), 32);
    assert!(first.chars().all(|c| c.is_ascii_hexdigit()));
    assert!(!first.contains("session-a"));
    let other = key(&bodies[2]).expect("another session key");
    assert_ne!(other, first);
    assert!(bodies[3].get("prompt_cache_key").is_none());
    assert!(bodies[4].get("prompt_cache_key").is_none());
    assert!(bodies[5].get("prompt_cache_key").is_none());
    drop(bodies);
    server.abort();
}

#[test]
fn compaction_cache_options_follow_the_gpt_model_version() {
    for (model, supported) in [
        ("gpt-5.5", false),
        ("gpt-5.6", true),
        ("openai/gpt-6-astra", true),
        ("other-model", false),
    ] {
        assert_eq!(
            super::supports_prompt_cache_options(model),
            supported,
            "{model}"
        );
    }
}

#[test]
fn matching_opaque_history_remains_readable_after_a_non_gpt_model_switches_to_summary_compaction() {
    use crate::services::compaction::native;
    let fixture = provider("openai", "openai-responses", "http://localhost");
    let history = [json!({"role":"user","provider_content":{"compaction":true,"provider":"test","base_url":"http://localhost","model":"fixture-model"}})];
    assert!(!native::supports(&fixture));
    native::validate_history(&history, &fixture).unwrap();
    let mut different = fixture;
    different.model.id = "different".into();
    assert!(native::validate_history(&history, &different).is_err());
}

#[test]
fn native_compaction_resolves_the_main_catalog_endpoint_and_excludes_aliases() {
    let mut p = provider("opencode-go", "openai-responses", "");
    p.model.id = "gpt-6-luna".into();
    p.model.api = None;
    assert!(crate::services::compaction::native::supports(&p));
    assert_eq!(
        catalog_base(&p).unwrap(),
        "https://opencode.ai/inference/go/openai/v1"
    );
    for alias in ["openai-codex-responses", "azure-openai-responses"] {
        p.model.api = Some(alias.into());
        assert!(!crate::services::compaction::native::supports(&p));
    }
}
