use super::*;
use crate::native_engine::{
    anthropic_client::{AnthropicClient, AnthropicContent, AnthropicMessage},
    openai_client::{OpenAIClient, OpenAIContent, OpenAIMessage},
    provider_manager::Provider,
    provider_runtime,
};
use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

fn provider(builtin: Option<&str>, url: &str, api: &str) -> ResolvedProvider {
    let provider: Provider = serde_json::from_value(json!({
        "id":"fixture", "name":"fixture", "base_url":url, "api_key":"fixture-only",
        "api_format":if api == "anthropic-messages" {"anthropic"} else {"openai"},
        "builtin_id":builtin, "enabled":true,
        "models":[{"id":"deepseek-third-party-model", "name":"fixture", "enabled":true,
            "max_tokens":1024, "supports_web_search":false, "api":api}]
    }))
    .unwrap();
    ResolvedProvider {
        model: provider.models[0].clone(),
        provider,
    }
}

#[test]
fn platform_uses_builtin_or_exact_hostname_not_model_or_url_substrings() {
    for api in ["openai-completions", "anthropic-messages"] {
        for url in [
            "https://deepseek.com",
            "https://api.deepseek.com/v1",
            "https://API.DEEPSEEK.COM:8443/anthropic",
            "https://a.b.deepseek.com",
        ] {
            assert!(is_deepseek(&provider(None, url, api)), "{url}");
        }
        for url in [
            "https://deepseek.com.evil.example",
            "https://notdeepseek.com",
            "https://deepseek.example",
            "https://openrouter.ai/deepseek.com",
            "https://deepseek.com@evil.example",
            "http://127.0.0.1",
            "not a URL",
        ] {
            assert!(!is_deepseek(&provider(None, url, api)), "{url}");
        }
        for builtin in ["opencode", "opencode-go", "openrouter", "DeepSeek"] {
            assert!(!is_deepseek(&provider(
                Some(builtin),
                "https://example.com",
                api
            )));
        }
        assert!(is_deepseek(&provider(
            Some("deepseek"),
            "http://127.0.0.1",
            api
        )));
    }
}

#[test]
fn invalid_session_id_is_rejected_without_rewriting_it() {
    let options = RequestOptions::for_session(Some("session\r\ninjected: true"));
    assert!(options
        .headers(&provider(
            Some("deepseek"),
            "http://localhost",
            "openai-completions"
        ))
        .is_err());
    assert!(options
        .headers(&provider(None, "http://localhost", "openai-completions"))
        .unwrap()
        .is_empty());
}

type Requests = Arc<Mutex<Vec<HeaderMap>>>;
async fn capture(
    State(requests): State<Requests>,
    headers: HeaderMap,
    Json(_): Json<Value>,
) -> (StatusCode, Json<Value>) {
    requests.lock().unwrap().push(headers);
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error":{"message":"fixture rejection"}})),
    )
}

async fn transport_matrix(channel: &str) {
    let requests: Requests = Default::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new()
        .fallback(post(capture))
        .with_state(requests.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = reqwest::Client::builder()
        .no_proxy()
        .resolve("api.deepseek.com", address)
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();
    let session = format!("MiXeD-Original-{}-END", "Ab09".repeat(80));
    let apis: &[&str] = if channel == "runtime" {
        &["openai-completions", "anthropic-messages"]
    } else if channel == "anthropic" {
        &["anthropic-messages"]
    } else {
        &["openai-completions"]
    };
    for api in apis {
        for deepseek in [true, false] {
            let (builtin, url) = if channel == "runtime" {
                (
                    Some(if deepseek { "deepseek" } else { "opencode-go" }),
                    format!("http://{address}"),
                )
            } else {
                (
                    None,
                    if deepseek {
                        format!("http://api.deepseek.com:{}", address.port())
                    } else {
                        format!("http://{address}")
                    },
                )
            };
            let p = provider(builtin, &url, api);
            for has_session in [true, false] {
                for compact in [true, false] {
                    let options = RequestOptions {
                        session_id: has_session.then(|| session.clone()),
                        compact,
                    };
                    let modes: &[&str] = match channel {
                        "anthropic" => &["single", "stream", "thinking"],
                        _ => &["single", "stream"],
                    };
                    for mode in modes {
                        let openai = || {
                            vec![OpenAIMessage {
                                role: "user".into(),
                                content: OpenAIContent::Text("hello".into()),
                                tool_calls: None,
                                tool_call_id: None,
                                reasoning_content: None,
                                provider_content: None,
                            }]
                        };
                        let anthropic = || {
                            vec![AnthropicMessage {
                                role: "user".into(),
                                content: AnthropicContent::Text("hello".into()),
                            }]
                        };
                        let failed = if channel == "anthropic" {
                            let c = AnthropicClient::new()
                                .with_http_client(client.clone())
                                .with_request_options(options.clone());
                            match *mode {
                                "single" => c
                                    .send_message(&p, anthropic(), None, vec![], 1024)
                                    .await
                                    .is_err(),
                                "stream" => c
                                    .send_message_stream(&p, anthropic(), None, vec![], 1024)
                                    .await
                                    .is_err(),
                                _ => c
                                    .send_message_stream_with_thinking(
                                        &p,
                                        anthropic(),
                                        None,
                                        vec![],
                                        1024,
                                        false,
                                    )
                                    .await
                                    .is_err(),
                            }
                        } else if channel == "runtime" && *mode == "stream" {
                            provider_runtime::stream_with_options(
                                &p,
                                openai(),
                                None,
                                vec![],
                                1024,
                                None,
                                None,
                                &options,
                            )
                            .await
                            .is_err()
                        } else {
                            let c = OpenAIClient::new()
                                .with_http_client(client.clone())
                                .with_request_options(options.clone());
                            if *mode == "single" {
                                c.send_message(&p, openai(), None, vec![], 1024)
                                    .await
                                    .is_err()
                            } else {
                                c.send_message_stream(&p, openai(), None, vec![], 1024)
                                    .await
                                    .is_err()
                            }
                        };
                        assert!(failed);
                        let captured = std::mem::take(&mut *requests.lock().unwrap());
                        assert_eq!(captured.len(), 1, "{channel}/{api}/{mode}");
                        let headers = &captured[0];
                        assert_eq!(
                            headers
                                .get("x-deepseek-harness-session-id")
                                .map(|v| v.to_str().unwrap()),
                            (deepseek && has_session).then_some(session.as_str()),
                            "{channel}/{api}/{mode}"
                        );
                        assert_eq!(
                            headers
                                .get("x-deepseek-harness-compact")
                                .map(|v| v.to_str().unwrap()),
                            (deepseek && compact).then_some("1")
                        );
                        assert_eq!(headers["content-type"], "application/json");
                        assert!(headers.contains_key("x-request-id"));
                        assert_eq!(headers["x-dcl-retry-count"], "0");
                        if *api == "anthropic-messages" {
                            assert_eq!(headers["anthropic-version"], "2023-06-01");
                            assert_eq!(headers["x-api-key"], "fixture-only");
                        } else {
                            assert_eq!(headers["authorization"], "Bearer fixture-only");
                        }
                        assert_eq!(
                            headers.contains_key("x-opencode-session"),
                            builtin == Some("opencode-go")
                        );
                    }
                }
            }
        }
    }
    server.abort();
}

#[tokio::test]
async fn openai_transport_harness_header_matrix() {
    transport_matrix("openai").await;
}
#[tokio::test]
async fn anthropic_transport_harness_header_matrix() {
    transport_matrix("anthropic").await;
}
#[tokio::test]
async fn builtin_runtime_harness_header_matrix_both_protocols() {
    transport_matrix("runtime").await;
}

#[tokio::test]
async fn summary_fragments_merges_and_truncation_recovery_keep_session_headers() {
    use crate::{runtime::RuntimePaths, services::compaction::checkpoint::Checkpoint};
    for api in ["openai-completions", "anthropic-messages"] {
        let requests = Arc::new(Mutex::new(Vec::<(HeaderMap, Value)>::new()));
        let captured = requests.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut p = provider(
            Some("deepseek"),
            &format!("http://{}", listener.local_addr().unwrap()),
            api,
        );
        p.model.context_window = Some(1_000_000);
        p.model.max_tokens = Some(4096);
        let app = Router::new().fallback(post(move |headers: HeaderMap, Json(body): Json<Value>| {
            let captured = captured.clone();
            async move {
                let index = { let mut requests = captured.lock().unwrap(); let index = requests.len(); requests.push((headers, body)); index };
                let text = format!("valid summary {index}");
                let events = if api == "openai-completions" {
                    vec![json!({"choices":[{"index":0,"delta":{"content":text},"finish_reason":if index == 0 {"length"} else {"stop"}}]})]
                } else {
                    vec![json!({"type":"message_start","message":{"id":"fixture","usage":{"input_tokens":10,"output_tokens":0}}}),
                        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
                        json!({"type":"message_delta","delta":{"stop_reason":if index == 0 {"max_tokens"} else {"end_turn"}},"usage":{"output_tokens":5}}),
                        json!({"type":"message_stop"})]
                };
                let mut stream = events.iter().map(|v|format!("data: {v}\n\n")).collect::<String>();
                if api == "openai-completions" {stream.push_str("data: [DONE]\n\n");}
                ([("content-type","text/event-stream")],stream)
            }
        }));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let root = tempfile::tempdir().unwrap();
        let db = RuntimePaths::new(root.path().join("data"))
            .unwrap()
            .open_database()
            .unwrap();
        let mut checkpoint =
            Checkpoint::open(&db, "Original-Compaction-Session", &p, "Summarize").unwrap();
        let input = (0..30_000)
            .map(|i| format!("Evidence {i}: preserve this fact.\n"))
            .collect::<String>();
        let result = crate::native_engine::generation::summarize(
            p,
            "Summarize",
            &input,
            Some(&mut checkpoint),
        )
        .await
        .unwrap();
        assert!(result.starts_with("valid summary"));
        server.abort();
        let requests = requests.lock().unwrap();
        assert!(requests.len() >= 7, "{api}: {} requests", requests.len());
        for (headers, _) in requests.iter() {
            assert_eq!(
                headers["x-deepseek-harness-session-id"],
                "Original-Compaction-Session"
            );
            assert_eq!(headers["x-deepseek-harness-compact"], "1");
        }
        let first = requests[0].1.to_string();
        assert!(first.contains("Summarize this consecutive fragment"));
        assert!(requests[1].1.to_string().len() < first.len());
        assert!(requests[2].1.to_string().len() < first.len());
        assert!(requests[3].1.to_string().contains("Merge these summaries"));
        assert!(requests
            .last()
            .unwrap()
            .1
            .to_string()
            .contains("Write one continuation summary"));
    }
}

#[tokio::test]
async fn auxiliary_generation_search_and_native_compaction_use_explicit_session() {
    let requests: Requests = Default::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let p = provider(
        Some("deepseek"),
        &format!("http://{}", listener.local_addr().unwrap()),
        "openai-completions",
    );
    let app = Router::new()
        .fallback(post(capture))
        .with_state(requests.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    assert!(crate::native_engine::generation::generate_for_session(
        p.clone(),
        "title",
        "input",
        Some("Title-Session")
    )
    .await
    .is_err());
    assert!(crate::native_engine::request_identity::TurnIdentity::new(
        Some("Ambient-Session".into()),
        None
    )
    .scope(crate::native_engine::generation::generate(
        p.clone(),
        "connection test",
        "input"
    ))
    .await
    .is_err());
    assert!(crate::tools::provider_search::search_for_session(
        json!({"query":"fixture"}),
        &p,
        Some("Search-Session")
    )
    .await
    .is_err());
    assert!(crate::services::compaction::native::generate_messages(
        &p,
        Some("Compact-Session"),
        vec![],
        "summarize"
    )
    .await
    .is_err());
    for api in [
        crate::native_engine::provider_manager::ApiFormat::OpenAI,
        crate::native_engine::provider_manager::ApiFormat::Anthropic,
    ] {
        let mut selected = p.clone();
        selected.provider.api_format = api;
        let classifier = crate::native_engine::auto_permissions::AutoClassifier::new(
            crate::permissions::auto_config::AutoConfig::default(),
            selected,
        )
        .with_session("Classifier-Session");
        let result = classifier
            .classify(&[], "Bash", &json!({"command":"echo fixture"}), ".")
            .await;
        assert_eq!(
            result.verdict.unwrap_err(),
            crate::native_engine::auto_permissions::Failure::Transport
        );
    }
    server.abort();
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 6);
    for (index, session) in [
        Some("Title-Session"),
        None,
        Some("Search-Session"),
        Some("Compact-Session"),
        Some("Classifier-Session"),
        Some("Classifier-Session"),
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(
            requests[index]
                .get("x-deepseek-harness-session-id")
                .map(|h| h.to_str().unwrap()),
            session
        );
        assert_eq!(
            requests[index]
                .get("x-deepseek-harness-compact")
                .map(|h| h.to_str().unwrap()),
            (index == 3).then_some("1")
        );
    }
}
