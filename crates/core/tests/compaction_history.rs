//! Compare the history seen by the main and compaction requests with durable rows.
use axum::{http::HeaderMap, response::IntoResponse, routing::post, Json, Router};
use deepseek_code_core::{
    cache::FileCache,
    native_engine::{
        engine_core::ChatRequest,
        provider_manager::{Provider, ProviderManager},
    },
    runtime::{self, RuntimePaths, SessionStore},
    services::compaction,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

#[path = "../src/native_engine/provider_runtime/bedrock.rs"]
mod bedrock;

struct Fixture {
    root: tempfile::TempDir,
    paths: RuntimePaths,
    store: SessionStore,
    id: String,
    provider: Provider,
    requests: Arc<Mutex<Vec<Value>>>,
    bodies: Arc<Mutex<Vec<axum::body::Bytes>>>,
    routes: Arc<Mutex<Vec<(String, HeaderMap)>>>,
    server: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn new(window: usize) -> Self {
        Self::with_root(window, tempfile::tempdir().unwrap()).await
    }

    async fn with_root(window: usize, root: tempfile::TempDir) -> Self {
        Self::with_answer(window, root, |_| Some("Pending work is recorded.")).await
    }

    async fn with_answer(
        window: usize,
        root: tempfile::TempDir,
        answer: fn(&Value) -> Option<&'static str>,
    ) -> Self {
        deepseek_code_core::config::OrchestrationConfig::init_global().unwrap();
        let paths = RuntimePaths::new(root.path().join("data")).unwrap();
        let store = SessionStore::new(paths.open_database().unwrap());
        let id = store.create(root.path(), "model", "fixture").unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = requests.clone();
        let routes = Arc::new(Mutex::new(Vec::new()));
        let seen_routes = routes.clone();
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let bytes = bodies.clone();
        let app = Router::new().fallback(post(move |uri: axum::http::Uri, headers: HeaderMap, raw: axum::body::Bytes| {
            let body: Value = serde_json::from_slice(&raw).unwrap();
            bytes.lock().unwrap().push(raw);
            let google = body.get("contents").is_some();
            let compact = headers["x-dcl-request-kind"] == "compaction";
            let cache_write = body.to_string().contains("cache write fixture");
            let stream = body["stream"] == true;
            let responses = body.get("input").is_some();
            let bedrock = body.get("inferenceConfig").is_some();
            let anthropic = headers.contains_key("anthropic-version");
            let structured = body["response_format"]["type"] == "json_schema" || body["messages"].to_string().contains("fixture always answers JSON");
            let text = if structured { Some(r#"{"ok":true}"#) } else if compact { answer(&body) } else { Some("last answer") };
            seen_routes.lock().unwrap().push((uri.path().to_owned(),headers));
            seen.lock().unwrap().push(body);
            async move {
                let Some(text) = text else {
                    return (axum::http::StatusCode::BAD_REQUEST, Json(json!({"error":{"code":"context_length_exceeded","message":"Maximum context length exceeded"}}))).into_response();
                };
                let usage = json!({"prompt_tokens":100,"completion_tokens":3,"total_tokens":103,"prompt_tokens_details":{"cached_tokens":if cache_write {75} else {80}, "cache_write_tokens":if cache_write {5} else {0}}});
                if uri.path().ends_with("/responses/compact") {
                    return Json(json!({"object":"response.compaction","output":[{"type":"compaction","encrypted_content":"opaque"}],"usage":{"input_tokens":100,"output_tokens":3}})).into_response();
                }
                if google {
                    let event = json!({"candidates":[{"content":{"role":"model","parts":[{"text":text}]},"finishReason":"STOP"}]});
                    return ([("content-type","text/event-stream")],format!("data: {event}\n\n")).into_response();
                }
                if bedrock {
                    let frames: Vec<u8> = [("messageStart",json!({"role":"assistant"})),
                        ("contentBlockDelta",json!({"contentBlockIndex":0,"delta":{"text":text}})),
                        ("messageStop",json!({"stopReason":"end_turn"})),
                        ("metadata",json!({"usage":{"inputTokens":20,"cacheReadInputTokens":75,"cacheWriteInputTokens":5,"outputTokens":3,"totalTokens":103}}))]
                        .into_iter().flat_map(|(kind, payload)| bedrock::frame(kind, payload)).collect();
                    ([("content-type","application/vnd.amazon.eventstream")],frames).into_response()
                } else if anthropic {
                    let frames = [json!({"type":"message_start","message":{"id":"m","usage":{"input_tokens":100}}}),
                        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
                        json!({"type":"content_block_stop","index":0}),
                        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}),
                        json!({"type":"message_stop"})].iter().map(|v|format!("data: {v}\n\n")).collect::<String>();
                    ([("content-type","text/event-stream")],frames).into_response()
                } else if responses {
                    let delta = json!({"type":"response.output_text.delta","delta":text});
                    let done = json!({"type":"response.completed","response":{"id":"r","status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}],"usage":{"input_tokens":100,"output_tokens":3}}});
                    ([("content-type","text/event-stream")],format!("data: {delta}\n\ndata: {done}\n\n")).into_response()
                } else if stream {
                    let delta = json!({"choices":[{"index":0,"delta":{"content":text},"finish_reason":"stop"}],"usage":usage});
                    ([("content-type","text/event-stream")],format!("data: {delta}\n\ndata: [DONE]\n\n")).into_response()
                } else {
                    Json(json!({"id":"summary","object":"chat.completion","created":0,"model":"model","choices":[{"index":0,"message":{"role":"assistant","content":text},"finish_reason":"stop"}],"usage":usage})).into_response()
                }
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let provider = serde_json::from_value(json!({"id":"fixture","name":"fixture","api_format":"openai","api_key":"fixture","base_url":format!("http://{addr}"),"enabled":true,"models":[{"id":"model","name":"fixture","enabled":true,"max_tokens":4096,"context_window":window,"supports_web_search":false}]})).unwrap();
        Self {
            root,
            paths,
            store,
            id,
            provider,
            requests,
            routes,
            bodies,
            server,
        }
    }

    fn engine(&self) -> deepseek_code_core::native_engine::engine_core::NativeEngine {
        runtime::create_engine(
            &self.paths,
            self.store.db.clone(),
            Arc::new(tokio::sync::Mutex::new(ProviderManager::in_memory(
                vec![self.provider.clone()],
                self.paths.data_dir.join("config/providers.json"),
            ))),
            runtime::default_permissions(),
            Arc::new(FileCache::new()),
        )
    }

    fn append(&self, role: &str, text: &str) {
        self.store.append(&self.id, role, text).unwrap();
    }

    async fn send_normal(&self, schema: Option<Value>) {
        self.send_selected(&self.provider.models[0].id, schema)
            .await;
    }

    async fn send_selected(&self, selector: &str, schema: Option<Value>) {
        let request: ChatRequest = serde_json::from_value(json!({"conversation_id":self.id,
            "messages":self.store.model_history(&self.id,&self.provider.api_format).unwrap(),
            "response_schema":schema,"system_prompt":"Custom session instructions","model":selector,"effort":self.provider.models[0].effort,"provider_id":"fixture","workspace_path":self.root.path()})).unwrap();
        let mut stream = self
            .engine()
            .send_message_with_history(request, false)
            .await
            .unwrap();
        while let Some(event) = stream.recv().await {
            assert!(
                !matches!(
                    event,
                    deepseek_code_core::native_engine::tool_loop::EngineEvent::Error(_)
                ),
                "{event:?}"
            );
        }
    }

    async fn main_turn(&self, schema: Option<Value>) {
        self.send_normal(schema.clone()).await;
        self.append(
            "assistant",
            if schema.is_some() {
                r#"{"ok":true}"#
            } else {
                "last answer"
            },
        );
    }

    async fn compact(&self) -> anyhow::Result<compaction::CompactionResult> {
        self.compact_selected(&self.provider.models[0].id).await
    }

    async fn compact_selected(
        &self,
        selector: &str,
    ) -> anyhow::Result<compaction::CompactionResult> {
        compaction::compact(
            &self.engine(),
            &self.store.db,
            &self.id,
            "fixture",
            selector,
            "Keep pending work",
        )
        .await
    }

    async fn finish(self) {
        self.server.abort();
        let _ = self.server.await;
    }
}

#[tokio::test]
async fn summary_uses_the_same_locally_compressed_history_as_the_next_turn() {
    let f = Fixture::new(50_000).await;
    let marker = "ORIGINAL_KEEP_MARKER";
    f.append("user", &format!("{}{marker}", "w".repeat(100_000)));
    for i in 0..5 {
        f.append("assistant", &format!("earlier answer {i}"));
        f.append("user", &format!("later question {i}"));
    }
    f.main_turn(None).await;
    f.compact().await.unwrap();
    {
        let calls = f.requests.lock().unwrap();
        assert!(!calls[0].to_string().contains(marker));
        assert_eq!(calls.len(), 2);
        assert!(!calls[1].to_string().contains(marker));
        assert!(calls[1].to_string().contains("later question 4"));
    }
    f.finish().await;
}

#[tokio::test]
async fn summary_replays_the_active_window_after_a_text_boundary() {
    let f = Fixture::new(128_000).await;
    f.append("user", "Original requirement preceding the boundary");
    f.append("assistant", "Acknowledged the original requirement.");
    let prepared = compaction::prepare(&f.store.db, &f.id).unwrap();
    compaction::commit(&f.store.db, prepared, "Prior continuation summary").unwrap();
    f.append("user", "New requirement following the boundary");
    f.compact().await.unwrap();
    {
        let calls = f.requests.lock().unwrap();
        let body = calls[0].to_string();
        assert!(!body.contains("Original requirement preceding the boundary"));
        for expected in [
            "Prior continuation summary",
            "New requirement following the boundary",
        ] {
            assert!(body.contains(expected));
        }
    }
    f.finish().await;
}

#[tokio::test]
async fn opaque_summary_replays_native_items_and_the_following_turns() {
    check_opaque_fallback(128_000, "").await;
}

#[tokio::test]
async fn segmented_fallback_preserves_the_window_in_each_request() {
    check_opaque_fallback(8192, &"w".repeat(40_000)).await;
}

async fn check_opaque_fallback(window: usize, padding: &str) {
    let mut f = Fixture::new(window).await;
    f.provider.builtin_id = Some("openai".into());
    f.provider.models[0].api = Some("openai-responses".into());
    f.append("user", "Original durable requirement");
    f.append("assistant", "Acknowledged the original requirement.");
    let items = json!([{"type":"compaction","id":"original","encrypted_content":"OPAQUE_WINDOW"}]);
    let shard = f.store.db.open_session(&f.id).unwrap();
    shard
        .write(|w| w.request_compaction("Keep the requirement", 50, 100))
        .unwrap();
    {
        let owner = shard.delivery_owner().unwrap();
        let claim = owner.claim_compaction().unwrap().unwrap();
        owner
            .finish_compaction(
                &claim,
                &compaction::GeneratedSummary {
                    text: "Native Responses compaction completed.".into(),
                    native: Some(
                        json!({"compaction":true,"api":"openai-responses","items":items,
                "provider":"fixture","model":"model","base_url":f.provider.base_url}),
                    ),
                },
            )
            .unwrap();
    }
    f.append(
        "user",
        &format!("Durable correction after boundary {padding}"),
    );
    f.compact().await.unwrap();
    {
        let calls = f.requests.lock().unwrap();
        assert!(!calls.is_empty());
        if !padding.is_empty() {
            assert!(calls.len() > 1);
        }
        assert!(calls.iter().all(|body| body["input"]
            .as_array()
            .is_some_and(|input| input.contains(&items[0]))));
        assert!(calls
            .iter()
            .all(|body| !body.to_string().contains("Original durable requirement")));
        assert!(calls.iter().any(|body| body
            .to_string()
            .contains("Durable correction after boundary")));
    }
    f.finish().await;
}

#[tokio::test]
async fn schema_constrained_main_turn_requires_a_text_summary_before_commit() {
    for stubborn in [false, true] {
        let f = Fixture::new(128_000).await;
        f.append(
            "user",
            if stubborn {
                "fixture always answers JSON"
            } else {
                "Continue work"
            },
        );
        f.main_turn(Some(json!({"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"],"additionalProperties":false}))).await;
        let before = f.store.history(&f.id).unwrap();
        let result = f.compact().await;
        let after = f.store.history(&f.id).unwrap();
        {
            let requests = f.requests.lock().unwrap();
            assert_eq!(requests[0]["response_format"]["type"], "json_schema");
            assert!(requests[1].get("response_format").is_none());
            assert_eq!(requests[0]["tools"], requests[1]["tools"]);
        }
        if stubborn {
            assert!(result.is_err());
            assert_eq!(before, after);
        } else {
            result.unwrap();
            assert!(after.last().unwrap()["content"]
                .as_str()
                .unwrap()
                .ends_with("Pending work is recorded."));
        }
        f.finish().await;
    }
}

#[tokio::test]
async fn first_iteration_peer_delivery_is_present_once_in_the_reused_prefix() {
    use deepseek_code_core::services::session_messages;
    // The inbox poll has a 250 ms deadline including the SQLite checkpoint.
    // Keep this fixture's database off a busy build disk where available.
    let root = tempfile::tempdir_in("/dev/shm")
        .or_else(|_| tempfile::tempdir())
        .unwrap();
    let f = Fixture::with_root(128_000, root).await;
    f.append("user", "Start the task");
    f.append("assistant", "Working on it");
    f.append("user", "Continue");
    let sender = f.store.fork(&f.id).unwrap();
    let marker = "UNIQUE_PEER_DELIVERY";
    session_messages::send(
        &session_messages::context(&f.paths.data_dir).unwrap(),
        &sender,
        &f.id,
        marker,
    )
    .unwrap();
    f.main_turn(None).await;
    assert_eq!(
        json!(f.store.history(&f.id).unwrap())
            .to_string()
            .matches(marker)
            .count(),
        1
    );
    f.compact().await.unwrap();
    {
        let calls = f.requests.lock().unwrap();
        assert_eq!(calls.len(), 2);
        for body in calls.iter() {
            assert_eq!(body.to_string().matches(marker).count(), 1);
            assert!(body["tools"].is_array());
        }
    }
    f.finish().await;
}

#[tokio::test]
async fn reasoning_enabled_claude_reuses_the_prefix_on_both_transports() {
    for (builtin, api, model) in [
        ("anthropic", "anthropic-messages", "claude-sonnet-4-5"),
        (
            "amazon-bedrock",
            "bedrock-converse-stream",
            "anthropic.claude-sonnet-4-5-20250929-v1:0",
        ),
    ] {
        let mut f = Fixture::new(128_000).await;
        f.provider.builtin_id = Some(builtin.into());
        f.provider.models[0].api = Some(api.into());
        f.provider.models[0].id = model.into();
        f.provider.models[0].effort = Some("medium".into());
        f.append("user", "Original question");
        f.append("assistant", "last answer");
        f.send_normal(None).await;
        f.compact().await.unwrap();
        {
            let calls = f.requests.lock().unwrap();
            assert_eq!(calls.len(), 2);
            let mut body = calls[1].clone();
            let messages = body["messages"].as_array_mut().unwrap();
            assert!(messages
                .pop()
                .unwrap()
                .to_string()
                .contains("Keep pending work"));
            assert!(messages.last().unwrap().to_string().contains("last answer"));
            assert_eq!(body, calls[0], "{api}");
        }
        f.finish().await;
    }
}

#[tokio::test]
async fn native_compaction_uses_the_main_cloudflare_route_and_authentication() {
    let mut f = Fixture::new(128_000).await;
    let vars = [
        ("CLOUDFLARE_ACCOUNT_ID", "account"),
        ("CLOUDFLARE_GATEWAY_ID", "gateway"),
    ];
    let previous = vars.map(|(name, _)| std::env::var_os(name));
    for (name, value) in vars {
        std::env::set_var(name, value);
    }
    f.provider.builtin_id = Some("cloudflare-ai-gateway".into());
    f.provider
        .base_url
        .push_str("/v1/{CLOUDFLARE_ACCOUNT_ID}/{CLOUDFLARE_GATEWAY_ID}/openai");
    f.provider.models[0].id = "gpt-5".into();
    f.provider.models[0].api = Some("openai-responses".into());
    f.append("user", "First request");
    f.append("assistant", "Earlier response");
    f.append("user", "Continue");
    f.main_turn(None).await;
    let result = f.compact().await;
    for ((name, _), old) in vars.into_iter().zip(previous) {
        if let Some(value) = old {
            std::env::set_var(name, value);
        } else {
            std::env::remove_var(name);
        }
    }
    result.unwrap();
    {
        let routes = f.routes.lock().unwrap();
        assert_eq!(routes.len(), 2);
        assert_eq!(routes[0].0, "/v1/account/gateway/openai/responses");
        assert_eq!(routes[1].0, format!("{}/compact", routes[0].0));
        for (_, headers) in routes.iter() {
            assert_eq!(headers["cf-aig-authorization"], "Bearer fixture");
            assert!(!headers.contains_key("authorization"));
        }
        let requests = f.requests.lock().unwrap();
        assert!(requests[0]["prompt_cache_key"].is_string());
        assert_eq!(
            requests[0]["prompt_cache_key"],
            requests[1]["prompt_cache_key"]
        );
    }
    f.finish().await;
}

#[tokio::test]
async fn daily_stats_count_cached_compaction_input_once() {
    let f = Fixture::new(128_000).await;
    f.append("user", "Initial request");
    f.append("assistant", "Initial response");
    f.compact().await.unwrap();
    {
        let stats = f.store.db.usage(|| false).unwrap();
        assert!(stats.failures.is_empty());
        assert_eq!(stats.days.len(), 1);
        assert_eq!(stats.days[0].tokens, [20, 3, 80, 0]);
        assert_eq!(stats.days[0].heat, 103);
        assert_eq!(stats.days[0].requests, 1);
    }
    f.finish().await;
}

#[tokio::test]
async fn fork_and_grandchild_share_one_physical_compaction_charge() {
    let f = Fixture::new(128_000).await;
    f.append("user", "Initial request");
    f.append("assistant", "Earlier reply");
    f.compact().await.unwrap();
    let child = f.store.fork(&f.id).unwrap();
    let grandchild = f.store.fork(&child).unwrap();
    let request = f.routes.lock().unwrap()[0].1["x-request-id"]
        .to_str()
        .unwrap()
        .to_owned();
    for session in [&f.id, &child, &grandchild] {
        let shard = f.store.db.open_session(session).unwrap();
        let metadata = shard
            .read(|r| r.request_metadata(&request))
            .unwrap()
            .expect("physical request ID in each fork");
        assert_eq!(metadata["kind"], "compaction");
        assert_eq!(shard.repository().usage_totals(&[]).unwrap().1, 100);
    }
    let usage = f.store.db.usage(|| false).unwrap();
    assert!(usage.failures.is_empty());
    assert_eq!(usage.days.len(), 1);
    assert_eq!(usage.days[0].session, f.id);
    assert_eq!(usage.days[0].requests, 1);
    assert_eq!(usage.days[0].heat, 103);
    f.finish().await;
}

#[tokio::test]
async fn bedrock_compaction_persists_inclusive_input_and_disjoint_cache_counts() {
    let mut f = Fixture::new(128_000).await;
    f.provider.builtin_id = Some("amazon-bedrock".into());
    f.provider.models[0].api = Some("bedrock-converse-stream".into());
    f.append("user", "First request");
    f.append("assistant", "Earlier reply");
    f.compact().await.unwrap();
    let (usage, input, output) = f
        .store
        .db
        .open_session(&f.id)
        .unwrap()
        .repository()
        .usage_totals(&[])
        .unwrap();
    assert_eq!((input, output), (100, 3));
    assert_eq!(usage["cache_read_tokens"], 75);
    assert_eq!(usage["cache_write_tokens"], 5);
    assert_eq!(
        f.store.db.usage(|| false).unwrap().days[0].tokens,
        [20, 3, 75, 5]
    );
    f.finish().await;
}

#[tokio::test]
async fn compaction_records_chat_cache_writes_in_their_own_bucket() {
    let f = Fixture::new(128_000).await;
    f.append("user", "cache write fixture");
    f.append("assistant", "Acknowledged.");
    f.compact().await.unwrap();
    let child = f.store.fork(&f.id).unwrap();
    let usage = f.store.db.usage(|| false).unwrap();
    assert!(usage.failures.is_empty());
    assert_eq!(usage.days.len(), 1);
    assert_eq!(usage.days[0].tokens, [20, 3, 75, 5]);
    let (details, input, _) = f
        .store
        .db
        .open_session(&child)
        .unwrap()
        .repository()
        .usage_totals(&[])
        .unwrap();
    assert_eq!(input, 100);
    assert_eq!(details["cache_write_tokens"], 5);
    f.finish().await;
}

#[tokio::test]
async fn historical_bedrock_daily_buckets_survive_a_compaction_fork() -> anyhow::Result<()> {
    let f = Fixture::new(128_000).await;
    let shard = f.store.db.open_session(&f.id)?;
    shard.write(|w| {
        let message = w.append("assistant", "Historical request")?;
        w.normalized_request(&message, &json!({"id":"old-bedrock","model":"model","apiFormat":"bedrock-converse-stream","startedAt":1,"inputTokens":20,"outputTokens":3,"cacheReadTokens":75,"cacheWriteTokens":5,"totalTokens":103,"usage":{"prompt_tokens":20,"completion_tokens":3,"total_tokens":103,"prompt_tokens_details":{"cached_tokens":75},"cache_creation_input_tokens":5}}))?;
        w.set_request_metadata("old-bedrock", &json!({"kind":"compaction"}))
    })?;
    f.store.fork(&f.id)?;
    let usage = f.store.db.usage(|| false)?;
    assert!(usage.failures.is_empty());
    assert_eq!(usage.days.len(), 1);
    let day = &usage.days[0];
    assert_eq!(
        (day.requests, day.heat, day.tokens),
        (1, 103, [20, 3, 75, 5])
    );
    f.finish().await;
    Ok(())
}

fn save_lookup(f: &Fixture, value: &str) -> anyhow::Result<String> {
    use deepseek_code_core::db::message_repo::ToolCallRow;
    let shard = f.store.db.open_session(&f.id)?;
    let message = shard.write(|w| w.append("assistant", ""))?;
    let display = shard.save_mcp(&message, "lookup", &json!({
        "content":[{"type":"text","text":"Lookup complete"}], "structuredContent":{"decision":value}
    }), false)?;
    shard.write(|w| {
        w.stream_tools(
            &message,
            "",
            &[ToolCallRow {
                id: "lookup".into(),
                message_id: message.clone(),
                name: "mcp__fixture__lookup".into(),
                input: Some("{}".into()),
                output: Some(display.clone()),
                is_error: false,
                sort_order: 0,
            }],
            &Default::default(),
        )
    })?;
    Ok(message)
}

#[tokio::test]
async fn compaction_replays_the_next_normal_body_on_every_protocol() -> anyhow::Result<()> {
    for (format, builtin, protocol, model, key) in [
        ("openai", None, None, "chat", "messages"),
        ("anthropic", None, None, "claude-sonnet-4-5", "messages"),
        (
            "openai",
            Some("openai"),
            Some("openai-completions"),
            "chat",
            "messages",
        ),
        (
            "openai",
            Some("mistral"),
            Some("mistral-conversations"),
            "model",
            "messages",
        ),
        (
            "openai",
            Some("azure-openai-responses"),
            Some("azure-openai-responses"),
            "model",
            "input",
        ),
        (
            "openai",
            Some("openai-codex"),
            Some("openai-codex-responses"),
            "model",
            "input",
        ),
        (
            "openai",
            Some("google-vertex"),
            Some("google-vertex"),
            "gemini-2.5-flash",
            "contents",
        ),
        (
            "openai",
            Some("openai"),
            Some("openai-responses"),
            "model",
            "input",
        ),
        (
            "openai",
            Some("anthropic"),
            Some("anthropic-messages"),
            "claude-sonnet-4-5",
            "messages",
        ),
        (
            "openai",
            Some("google"),
            Some("google-generative-ai"),
            "gemini-2.5-flash",
            "contents",
        ),
        (
            "openai",
            Some("amazon-bedrock"),
            Some("bedrock-converse-stream"),
            "anthropic.claude-sonnet-4-5-20250929-v1:0",
            "messages",
        ),
    ] {
        let mut f = Fixture::new(128_000).await;
        f.provider.api_format = serde_json::from_value(json!(format))?;
        f.provider.builtin_id = builtin.map(String::from);
        if protocol == Some("openai-codex-responses") {
            use base64::Engine;
            let claim = json!({"https://api.openai.com/auth":{"chatgpt_account_id":"fixture"}});
            let payload =
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claim.to_string());
            f.provider.api_key = format!("fixture.{payload}.fixture");
        }
        f.provider.models[0].api = protocol.map(String::from);
        f.provider.models[0].id = model.into();
        f.provider.models[0].effort = Some("medium".into());
        f.append("user", "DISCARDED_PLAN_GATE");
        let shard = f.store.db.open_session(&f.id)?;
        let context = deepseek_code_core::services::ServiceContext {
            paths: f.paths.clone(),
            db: f.store.db.clone(),
            credentials: Arc::new(deepseek_code_core::services::EnvironmentCredentials),
        };
        let memory =
            deepseek_code_core::services::memory_files::FileMemory::session(&context, &f.id)?;
        memory.put("saffron.md", "---\nname: saffron\ndescription: saffron\nmetadata:\n  type: user\n---\nADMITTED_RECALL_MARKER\n", None)?;
        let index = memory.directory.join("MEMORY.md");
        std::fs::write(&index, "FROZEN_PLAN_INDEX")?;
        shard.approve_plan("APPROVED_RELEASE_PLAN")?;
        std::fs::write(&index, "CURRENT_RECEIPT_INDEX")?;
        shard.admit_prompt("Attached release.txt", Some(&json!([{"type":"attachment","fileName":"release.txt","size":20,"source":{"type":"base64","media_type":"text/plain","data":"REVQTE9ZX09OTFlfQUZURVJfUUE="}}])), "typed")?;
        save_lookup(&f, "STRUCTURED_DECISION")?;
        shard.admit_prompt("Summarize the saffron decision", None, "typed")?;
        std::fs::write(&index, "LATER_DISK_INDEX")?;
        std::fs::write(memory.directory.join("saffron.md"), "LATER_DISK_RECALL")?;
        f.send_normal(None).await;
        f.compact().await?;
        {
            let bodies = f.bodies.lock().unwrap();
            assert_eq!(bodies.len(), 2, "{model}");
            let compact = std::str::from_utf8(&bodies[1])?;
            let mut parsed: Value = serde_json::from_str(compact)?;
            let instruction = parsed[key].as_array_mut().unwrap().pop().unwrap();
            assert!(instruction.to_string().contains("Keep pending work"));
            let suffix = format!(",{}", serde_json::to_string(&instruction)?);
            assert_eq!(compact.matches(&suffix).count(), 1);
            assert_eq!(
                compact.replacen(&suffix, "", 1),
                std::str::from_utf8(&bodies[0])?,
                "{model} {protocol:?}"
            );
            for marker in [
                "APPROVED_RELEASE_PLAN",
                "DEPLOY_ONLY_AFTER_QA",
                "STRUCTURED_DECISION",
                "Custom session instructions",
            ] {
                assert!(compact.contains(marker), "{model}: missing {marker}");
            }
            for marker in ["FROZEN_PLAN_INDEX", "LATER_DISK_INDEX"] {
                assert_eq!(compact.matches(marker).count(), 1, "{model} {marker}");
            }
            for marker in [
                "DISCARDED_PLAN_GATE",
                "CURRENT_RECEIPT_INDEX",
                "ADMITTED_RECALL_MARKER",
                "LATER_DISK_RECALL",
            ] {
                assert!(!compact.contains(marker), "{model} {marker}");
            }
        }
        f.finish().await;
    }
    Ok(())
}

#[tokio::test]
async fn compaction_conflict_guard_and_partial_summary_include_the_structured_result(
) -> anyhow::Result<()> {
    let f = Fixture::new(128_000).await;
    f.append("user", "Initial request");
    let message = save_lookup(&f, "PENDING_REVIEW")?;
    let shard = f.store.db.open_session(&f.id)?;
    shard.write(|w| w.request_compaction("keep approval", 50, 100))?;
    let owner = shard.delivery_owner()?;
    let queued = owner.claim_compaction()?.unwrap();
    let prepared = compaction::prepare(&f.store.db, &f.id)?;
    shard.save_mcp(&message, "lookup", &json!({
        "content":[{"type":"text","text":"Lookup complete"}], "structuredContent":{"decision":"APPROVED_REVIEW"}
    }), false)?;
    for error in [
        compaction::commit(&f.store.db, prepared, "Still pending").unwrap_err(),
        owner
            .finish_compaction(&queued, &"Still pending".into())
            .unwrap_err(),
    ] {
        let cause = format!("{error:#}");
        assert!(
            cause.contains("Session changed during compaction"),
            "{cause}"
        );
    }
    drop(owner);
    compaction::summarize_part(
        &f.engine(),
        &f.store.db,
        &f.id,
        &message,
        compaction::Part::From,
        "fixture",
        "model",
    )
    .await?;
    let body = f.requests.lock().unwrap()[0].to_string();
    assert!(body.contains("APPROVED_REVIEW"));
    assert!(!body.contains("Initial request"));
    f.finish().await;
    Ok(())
}

#[tokio::test]
async fn replay_preserves_the_virtual_context_window() -> anyhow::Result<()> {
    const GATE: &str = "REQUIRES_QA_42";
    let f = Fixture::with_answer(50_000, tempfile::tempdir()?, |body| {
        if body["messages"].to_string().contains(GATE) {
            Some(GATE)
        } else {
            Some("No QA gate was recorded.")
        }
    })
    .await;
    f.append("user", &format!("{}{GATE}", "w".repeat(100_000)));
    for i in 0..5 {
        f.append("assistant", &format!("Earlier response {i}"));
        f.append("user", &format!("Follow-up {i}"));
    }
    f.send_selected("model[1m]", None).await;
    f.compact_selected("model[1m]").await?;
    let history = f.store.model_history(&f.id, &f.provider.api_format)?;
    {
        let calls = f.requests.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(calls[0]["messages"].to_string().contains(GATE));
        assert!(
            calls[1]["messages"].to_string().contains(GATE),
            "summary lost the QA gate"
        );
    }
    assert!(history.last().unwrap()["content"]
        .as_str()
        .unwrap()
        .contains(GATE));
    f.finish().await;
    Ok(())
}

fn has_typed_image(body: &Value) -> bool {
    body["messages"].as_array().into_iter().flatten().any(|m| {
        m["content"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|p| p["type"] == "image_url")
    })
}

const VISUAL_FINDING: &str = "Red light is visible.";

fn visual_answer(body: &Value) -> Option<&'static str> {
    Some(
        if has_typed_image(body) || body["messages"].to_string().contains(VISUAL_FINDING) {
            VISUAL_FINDING
        } else {
            "No visual evidence was supplied."
        },
    )
}

async fn visual_fixture(
    filename: &str,
    answer: fn(&Value) -> Option<&'static str>,
) -> anyhow::Result<Fixture> {
    let mut f = Fixture::with_answer(128_000, tempfile::tempdir()?, answer).await;
    f.provider.models[0].supports_vision = true;
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(filename);
    let image = runtime::image_block_from_file(&path)?;
    f.store.db.open_session(&f.id)?.admit_prompt(
        "Status light",
        Some(&json!([{"type":"text","text":"Record the status light in this image."},image])),
        "typed",
    )?;
    f.append("assistant", "Acknowledged.");
    Ok(f)
}

#[tokio::test]
async fn replay_preserves_the_visual_finding_for_both_png_encodings() -> anyhow::Result<()> {
    for filename in ["red-compressed.png", "red-uncompressed.png"] {
        let f = visual_fixture(filename, visual_answer).await?;
        f.send_normal(None).await;
        f.compact().await?;
        let history = f.store.model_history(&f.id, &f.provider.api_format)?;
        {
            let calls = f.requests.lock().unwrap();
            assert!(has_typed_image(&calls[0]));
            assert!(
                calls[1..].iter().any(has_typed_image),
                "no typed image in summary requests with {filename}"
            );
            assert_eq!(calls.len(), 2, "PNG encoding changed the whole-request fit");
        }
        assert!(history.last().unwrap()["content"]
            .as_str()
            .unwrap()
            .contains(VISUAL_FINDING));
        f.finish().await;
    }
    Ok(())
}

#[tokio::test]
async fn visual_recovery_preserves_the_image_or_retains_active_history() -> anyhow::Result<()> {
    for reject_image in [false, true] {
        let f = if reject_image {
            visual_fixture("red-compressed.png", |body| {
                if has_typed_image(body) {
                    None
                } else {
                    visual_answer(body)
                }
            })
            .await?
        } else {
            visual_fixture("red-compressed.png", |body| {
                if body["tools"].is_array() {
                    None
                } else {
                    visual_answer(body)
                }
            })
            .await?
        };
        f.send_normal(None).await;
        let before = f.store.model_history(&f.id, &f.provider.api_format)?;
        let result = f.compact().await;
        let after = f.store.model_history(&f.id, &f.provider.api_format)?;
        {
            let calls = f.requests.lock().unwrap();
            assert!(has_typed_image(&calls[0]));
            assert!(has_typed_image(&calls[1]));
            assert!(
                calls[2..].iter().any(has_typed_image),
                "the image must reach recovery"
            );
            for body in &calls[2..] {
                if body["messages"].to_string().contains("attached_image") {
                    assert!(has_typed_image(body));
                }
            }
        }
        if reject_image {
            let error = result
                .err()
                .expect("an unrepresentable image must fail compaction");
            assert!(format!("{error:#}").contains("original history retained"));
            assert!(after == before, "rejected media changed the active history");
        } else {
            result?;
            assert!(after.last().unwrap()["content"]
                .as_str()
                .unwrap()
                .contains(VISUAL_FINDING));
        }
        f.finish().await;
    }
    Ok(())
}
