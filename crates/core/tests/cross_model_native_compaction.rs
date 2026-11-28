use axum::{
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
    Json, Router,
};
use deepseek_code_core::{
    cache::FileCache,
    native_engine::{
        engine_core::ChatRequest,
        provider_manager::{ApiFormat, Provider, ProviderManager},
        tool_loop::EngineEvent,
        NativeEngine,
    },
    runtime::{self, RuntimePaths, SessionStore},
    services::compaction,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

fn gpt_native_window() -> Value {
    json!([
        {"role":"user","content":[{"type":"input_text","text":"Finish 中文 tests"}]},
        {"type":"reasoning","id":"reasoning-id","encrypted_content":"exact signed reasoning"},
        {"type":"compaction","id":"compact-id","encrypted_content":"opaque exact content"}
    ])
}

#[derive(Clone, Debug)]
struct RecordedCall {
    endpoint: &'static str,
    is_compaction: bool,
    body: Value,
}

struct Fixture {
    dir: tempfile::TempDir,
    store: SessionStore,
    id: String,
    engine: NativeEngine,
    calls: Arc<Mutex<Vec<RecordedCall>>>,
    server: tokio::task::JoinHandle<()>,
}

async fn fixture() -> Fixture {
    deepseek_code_core::config::OrchestrationConfig::init_global().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(dir.path().join("data")).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let id = store.create(dir.path(), "gpt-6-astra", "gateway").unwrap();
    store.append(&id, "user", "Finish 中文 tests").unwrap();
    store
        .append(&id, "assistant", "Tests remain pending")
        .unwrap();

    let calls = Arc::new(Mutex::new(Vec::new()));

    let compact_calls = calls.clone();
    let responses_calls = calls.clone();
    let chat_calls = calls.clone();

    let app = Router::new()
        .route(
            "/tools/codex/v1/responses/compact",
            post(move |Json(body): Json<Value>| {
                let calls = compact_calls.clone();
                async move {
                    let model = body["model"].as_str().unwrap_or_default().to_owned();
                    calls.lock().unwrap().push(RecordedCall {
                        endpoint: "responses/compact",
                        is_compaction: true,
                        body,
                    });
                    if model != "gpt-6-astra" {
                        return (
                            StatusCode::NOT_FOUND,
                            Json(json!({"error":"native compact unsupported for model"})),
                        )
                            .into_response();
                    }
                    (
                        StatusCode::OK,
                        Json(json!({
                            "object": "response.compaction",
                            "output": gpt_native_window(),
                            "usage": {"input_tokens": 150, "output_tokens": 77}
                        })),
                    )
                        .into_response()
                }
            }),
        )
        .route(
            "/tools/codex/v1/responses",
            post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let calls = responses_calls.clone();
                async move {
                    let is_compaction = headers
                        .get("x-dcl-request-kind")
                        .is_some_and(|kind| kind == "compaction");
                    let reply = if is_compaction {
                        "RECOMPACTED_FOR_DEEPSEEK_RESPONSES"
                    } else {
                        "DEEPSEEK_RESPONSES_REPLY"
                    };
                    calls.lock().unwrap().push(RecordedCall {
                        endpoint: "responses",
                        is_compaction,
                        body,
                    });
                    let data = json!({
                        "type": "response.completed",
                        "response": {
                            "id": "resp-1",
                            "status": "completed",
                            "output": [{"type":"message","role":"assistant","content":[{"type":"output_text","text":reply}]}],
                            "usage": {"input_tokens": 60, "output_tokens": 12}
                        }
                    });
                    let delta = format!(
                        "event: response.output_text.delta\ndata: {}\n\n",
                        json!({"type":"response.output_text.delta","delta":reply})
                    );
                    (
                        [("Content-Type", "text/event-stream")],
                        format!("{delta}event: response.completed\ndata: {data}\n\n"),
                    )
                }
            }),
        )
        .route(
            "/deepseek/v1/chat/completions",
            post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let calls = chat_calls.clone();
                async move {
                    let is_compaction = headers
                        .get("x-dcl-request-kind")
                        .is_some_and(|kind| kind == "compaction");
                    let reply = if is_compaction {
                        "RECOMPACTED_FOR_DEEPSEEK_CHAT"
                    } else {
                        "DEEPSEEK_CHAT_REPLY"
                    };
                    calls.lock().unwrap().push(RecordedCall {
                        endpoint: "chat/completions",
                        is_compaction,
                        body,
                    });
                    let chunk = json!({
                        "id": "chatcmpl-1",
                        "choices": [{"index": 0, "delta": {"content": reply}, "finish_reason": null}]
                    });
                    (
                        [("content-type", "text/event-stream")],
                        format!(
                            "data: {chunk}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\ndata: [DONE]\n\n"
                        ),
                    )
                }
            }),
        );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let gateway: Provider = serde_json::from_value(json!({
        "id": "gateway",
        "name": "gateway",
        "builtin_id": "openai",
        "api_format": "openai",
        "base_url": format!("http://{addr}/tools/codex/v1"),
        "api_key": "gateway-key",
        "enabled": true,
        "models": [
            {"id": "gpt-6-astra", "name": "gpt-6-astra", "api": "openai-responses", "enabled": true, "supports_web_search": false, "max_tokens": 8192, "context_window": 256000},
            {"id": "deepseek-v4.1-flash", "name": "deepseek-v4.1-flash", "api": "openai-responses", "enabled": true, "supports_web_search": false, "max_tokens": 8192, "context_window": 256000}
        ]
    }))
    .unwrap();
    let deepseek: Provider = serde_json::from_value(json!({
        "id": "deepseek",
        "name": "deepseek",
        "api_format": "openai",
        "base_url": format!("http://{addr}/deepseek/v1"),
        "api_key": "deepseek-key",
        "enabled": true,
        "models": [
            {"id": "deepseek-chat", "name": "deepseek-chat", "enabled": true, "supports_web_search": false, "max_tokens": 8192, "context_window": 128000}
        ]
    }))
    .unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let permissions = runtime::default_permissions();
    permissions.set_mode(deepseek_code_core::permissions::PermissionMode::BypassPermissions);
    let engine = runtime::create_engine(
        &paths,
        store.db.clone(),
        Arc::new(tokio::sync::Mutex::new(ProviderManager::in_memory(
            vec![gateway, deepseek],
            paths.data_dir.join("config/providers.json"),
        ))),
        permissions,
        Arc::new(FileCache::new()),
    );

    Fixture {
        dir,
        store,
        id,
        engine,
        calls,
        server,
    }
}

async fn drain_turn(mut events: tokio::sync::mpsc::Receiver<EngineEvent>) -> Vec<EngineEvent> {
    let mut collected = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Some(event) = events.recv().await {
            assert!(!format!("{event:?}").starts_with("Error"), "{event:?}");
            collected.push(event);
        }
    })
    .await
    .unwrap();
    collected
}

#[tokio::test]
async fn gpt_native_compaction_then_switch_to_deepseek_responses_recompacts_and_continues() {
    let Fixture {
        dir,
        store,
        id,
        engine,
        calls,
        server,
    } = fixture().await;

    compaction::compact(
        &engine,
        &store.db,
        &id,
        "gateway",
        "gpt-6-astra",
        "Keep pending work",
    )
    .await
    .unwrap();

    store
        .append(&id, "user", "Continue on DeepSeek Responses")
        .unwrap();
    let stale_history = store.model_history(&id, &ApiFormat::OpenAI).unwrap();
    assert_eq!(stale_history[0]["provider_content"]["compaction"], true);

    let request: ChatRequest = serde_json::from_value(json!({
        "conversation_id": id,
        "messages": stale_history,
        "model": "deepseek-v4.1-flash",
        "provider_id": "gateway",
        "workspace_path": dir.path(),
        "max_tokens": 8192
    }))
    .unwrap();
    let events = drain_turn(engine.send_message(request).await.unwrap()).await;
    let notice = events
        .iter()
        .find_map(|event| match event {
            EngineEvent::CompactionNotice(msg) if msg.contains("re-compacted") => Some(msg),
            _ => None,
        })
        .expect("expected re-compaction CompactionNotice");
    assert!(notice.contains("Summary saved to:"), "{notice}");

    let recorded = calls.lock().unwrap().clone();
    let summary_call = recorded
        .iter()
        .find(|c| c.endpoint == "responses" && c.is_compaction)
        .expect("expected DeepSeek summary fallback request");
    let summary_input = summary_call.body["input"].to_string();
    assert!(
        summary_input.contains("Finish 中文 tests"),
        "{summary_input}"
    );
    assert!(
        summary_input.contains("Tests remain pending"),
        "{summary_input}"
    );
    assert!(
        !summary_input.contains("Summary focus requested by the user:"),
        "{summary_input}"
    );
    assert!(
        !summary_input.contains("opaque exact content"),
        "{summary_input}"
    );
    assert!(
        !summary_input.contains("Continue on DeepSeek Responses"),
        "{summary_input}"
    );

    let turn_call = recorded
        .iter()
        .find(|c| c.endpoint == "responses" && !c.is_compaction)
        .expect("expected DeepSeek Responses turn request");
    assert_eq!(turn_call.body["model"], "deepseek-v4.1-flash");
    let turn_input = turn_call.body["input"].to_string();
    assert!(
        turn_input.contains("RECOMPACTED_FOR_DEEPSEEK_RESPONSES"),
        "{turn_input}"
    );
    assert!(
        turn_input.contains("Continue on DeepSeek Responses"),
        "{turn_input}"
    );
    assert!(!turn_input.contains("opaque exact content"), "{turn_input}");

    let updated_history = json!(store.model_history(&id, &ApiFormat::OpenAI).unwrap()).to_string();
    assert!(
        !updated_history.contains("opaque exact content"),
        "{updated_history}"
    );
    assert!(
        updated_history.contains("RECOMPACTED_FOR_DEEPSEEK_RESPONSES"),
        "{updated_history}"
    );

    let export_dir = dir
        .path()
        .join("data")
        .join("cache")
        .join("compactions")
        .join(&id);
    let md_paths: Vec<std::path::PathBuf> = std::fs::read_dir(&export_dir)
        .unwrap()
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            (path.extension().and_then(|e| e.to_str()) == Some("md")).then_some(path)
        })
        .collect();
    let normalize = |raw: &str| raw.replace('\\', "/").replace("//?/", "");
    let normalized_notice = normalize(notice);
    assert!(
        md_paths
            .iter()
            .any(|path| normalized_notice.contains(&normalize(&path.display().to_string()))),
        "notice missing exported path: {notice} ({md_paths:?})"
    );
    let exported: Vec<String> = md_paths
        .iter()
        .map(|path| std::fs::read_to_string(path).unwrap())
        .collect();
    assert!(
        exported
            .iter()
            .any(|text| text.contains("RECOMPACTED_FOR_DEEPSEEK_RESPONSES")),
        "{exported:?}"
    );
    assert!(!dir.path().join("data/data/cache/compactions").exists());

    server.abort();
}

#[tokio::test]
async fn gpt_native_compaction_with_followup_then_switch_to_deepseek_chat_completions() {
    let Fixture {
        dir,
        store,
        id,
        engine,
        calls,
        server,
    } = fixture().await;

    compaction::compact(
        &engine,
        &store.db,
        &id,
        "gateway",
        "gpt-6-astra",
        "Keep pending work",
    )
    .await
    .unwrap();

    // Simulate a follow-up GPT turn persisted after native compaction before switching models.
    store.append(&id, "user", "Followup after compact").unwrap();
    store
        .append(&id, "assistant", "Followup GPT reply")
        .unwrap();
    store
        .append(&id, "user", "Now answer in DeepSeek Chat")
        .unwrap();

    let stale_history = store.model_history(&id, &ApiFormat::OpenAI).unwrap();
    let request: ChatRequest = serde_json::from_value(json!({
        "conversation_id": id,
        "messages": stale_history,
        "model": "deepseek-chat",
        "provider_id": "deepseek",
        "workspace_path": dir.path(),
        "max_tokens": 8192
    }))
    .unwrap();
    let events = drain_turn(engine.send_message(request).await.unwrap()).await;
    assert!(events.iter().any(|event| matches!(
        event,
        EngineEvent::CompactionNotice(msg) if msg.contains("plain-text summary for deepseek-chat")
    )));

    let recorded = calls.lock().unwrap().clone();
    let summary_call = recorded
        .iter()
        .find(|c| c.endpoint == "chat/completions" && c.is_compaction)
        .expect("expected DeepSeek chat compaction request");
    assert_eq!(summary_call.body["model"], "deepseek-chat");
    let summary_msgs = summary_call.body["messages"].to_string();
    assert!(summary_msgs.contains("Finish 中文 tests"), "{summary_msgs}");
    assert!(
        summary_msgs.contains("Tests remain pending"),
        "{summary_msgs}"
    );
    assert!(
        !summary_msgs.contains("Followup after compact"),
        "{summary_msgs}"
    );
    assert!(
        !summary_msgs.contains("opaque exact content"),
        "{summary_msgs}"
    );

    let turn_call = recorded
        .iter()
        .find(|c| c.endpoint == "chat/completions" && !c.is_compaction)
        .expect("expected DeepSeek chat turn request");
    assert_eq!(turn_call.body["model"], "deepseek-chat");
    let turn_msgs = turn_call.body["messages"].to_string();
    assert!(
        turn_msgs.contains("RECOMPACTED_FOR_DEEPSEEK_CHAT"),
        "{turn_msgs}"
    );
    assert!(turn_msgs.contains("Followup after compact"), "{turn_msgs}");
    assert!(turn_msgs.contains("Followup GPT reply"), "{turn_msgs}");
    assert!(
        turn_msgs.contains("Now answer in DeepSeek Chat"),
        "{turn_msgs}"
    );
    assert!(!turn_msgs.contains("opaque exact content"), "{turn_msgs}");

    server.abort();
}

#[tokio::test]
async fn manual_compact_after_switching_from_gpt_native_to_deepseek_succeeds() {
    let Fixture {
        dir: _dir,
        store,
        id,
        engine,
        calls,
        server,
    } = fixture().await;

    compaction::compact(
        &engine,
        &store.db,
        &id,
        "gateway",
        "gpt-6-astra",
        "Keep pending work",
    )
    .await
    .unwrap();

    store.append(&id, "user", "Work after compact").unwrap();
    store.append(&id, "assistant", "Work result").unwrap();

    let result = compaction::compact(
        &engine,
        &store.db,
        &id,
        "deepseek",
        "deepseek-chat",
        "Keep post-switch summary",
    )
    .await
    .unwrap();
    assert!(!result.message_id.is_empty());

    let recorded = calls.lock().unwrap().clone();
    let chat_compact = recorded
        .iter()
        .find(|c| c.endpoint == "chat/completions" && c.is_compaction)
        .expect("expected DeepSeek manual compact request");
    let body_text = chat_compact.body.to_string();
    assert!(body_text.contains("Finish 中文 tests"), "{body_text}");
    assert!(body_text.contains("Work after compact"), "{body_text}");
    assert!(body_text.contains("Work result"), "{body_text}");
    assert!(
        body_text.contains("Summary focus requested by the user:"),
        "{body_text}"
    );
    assert!(
        body_text.contains("Keep post-switch summary"),
        "{body_text}"
    );
    assert!(!body_text.contains("opaque exact content"), "{body_text}");

    let refreshed = json!(store.model_history(&id, &ApiFormat::OpenAI).unwrap()).to_string();
    assert!(!refreshed.contains("opaque exact content"), "{refreshed}");
    assert!(
        refreshed.contains("RECOMPACTED_FOR_DEEPSEEK_CHAT"),
        "{refreshed}"
    );

    server.abort();
}
