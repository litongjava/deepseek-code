//! End-to-end tests for mid-turn automatic context compaction during long tool loops.
use axum::{
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
    Json, Router,
};
use deepseek_code_core::{
    cache::FileCache,
    db::session_store::settings::{Preferences, PREFERENCES},
    native_engine::{
        engine_core::ChatRequest,
        provider_manager::{ApiFormat, Provider, ProviderManager},
        tool_loop::{ContextResetReason, EngineEvent},
    },
    permissions::PermissionMode,
    runtime::{self, RuntimePaths, SessionStore},
};
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use tokio::sync::Notify;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Protocol {
    OpenAIChat,
    Anthropic,
    OpenAIResponses,
}

fn openai_sse(delta: Value, finish: &str, prompt_tokens: u64) -> String {
    let first = json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]});
    let second = json!({
        "choices":[{"index":0,"delta":{},"finish_reason":finish}],
        "usage":{"prompt_tokens":prompt_tokens,"completion_tokens":10,"total_tokens":prompt_tokens + 10}
    });
    format!("data: {first}\n\ndata: {second}\n\ndata: [DONE]\n\n")
}

fn anthropic_tool_sse(call_id: &str, name: &str, input: &Value, input_tokens: u64) -> String {
    let events = [
        json!({"type":"message_start","message":{"id":"m","usage":{"input_tokens":input_tokens}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":call_id,"name":name,"input":{}}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":input.to_string()}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":10}}),
        json!({"type":"message_stop"}),
    ];
    events.iter().map(|e| format!("data: {e}\n\n")).collect()
}

fn anthropic_text_sse(text: &str, input_tokens: u64) -> String {
    let events = [
        json!({"type":"message_start","message":{"id":"m","usage":{"input_tokens":input_tokens}}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
        json!({"type":"content_block_stop","index":0}),
        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":10}}),
        json!({"type":"message_stop"}),
    ];
    events.iter().map(|e| format!("data: {e}\n\n")).collect()
}

fn responses_sse(output: Value, delta_text: Option<&str>, input_tokens: u64) -> String {
    let mut frames = String::new();
    if let Some(text) = delta_text {
        let delta = json!({"type":"response.output_text.delta","delta":text});
        frames.push_str(&format!("event: response.output_text.delta\ndata: {delta}\n\n"));
    }
    let done = json!({
        "type":"response.completed",
        "response":{
            "id":"resp-1",
            "status":"completed",
            "output":output,
            "usage":{"input_tokens":input_tokens,"output_tokens":10}
        }
    });
    frames.push_str(&format!("event: response.completed\ndata: {done}\n\n"));
    frames
}

async fn run_mid_turn_scenario(
    protocol: Protocol,
    prefs: Preferences,
    standalone_frontend: bool,
) -> (
    tempfile::TempDir,
    SessionStore,
    String,
    Vec<(String, HeaderMap, Value)>,
    Vec<EngineEvent>,
) {
    deepseek_code_core::config::OrchestrationConfig::init_global().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let big_file = dir.path().join("evidence.txt");
    // ~45 KB -> ~15,000 estimated tokens; with ~4-5k tool overhead this crosses the
    // 50% threshold (16,000) of a 32,000 window while still fitting one whole-request summary.
    let marker_line = "BULKY_EVIDENCE_MARKER_0123456789 ".repeat(1400);
    std::fs::write(&big_file, &marker_line).unwrap();

    let paths = RuntimePaths::new(dir.path().join("data")).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let model_id = match protocol {
        Protocol::OpenAIResponses => "gpt-6-astra",
        _ => "fixture-model",
    };
    let id = store.create(dir.path(), model_id, "fixture").unwrap();
    store
        .db
        .open_session(&id)
        .unwrap()
        .write(|w| w.set_setting(PREFERENCES, &prefs))
        .unwrap();
    store
        .append(&id, "user", "Inspect evidence.txt and summarize findings")
        .unwrap();

    let turn = store.db.start_turn_timer(&id).unwrap();
    let mut turn = turn;
    let assistant_id = turn.ensure_segment().unwrap();

    let recorded: Arc<Mutex<Vec<(String, HeaderMap, Value)>>> = Arc::default();
    let recorded_clone = recorded.clone();
    let main_step = Arc::new(AtomicUsize::new(0));
    let main_step_clone = main_step.clone();
    let file_path = big_file.to_string_lossy().into_owned();

    let app = Router::new().fallback(post(
        move |uri: axum::http::Uri, headers: HeaderMap, Json(body): Json<Value>| {
            let recorded = recorded_clone.clone();
            let main_step = main_step_clone.clone();
            let file_path = file_path.clone();
            async move {
                let path = uri.path().to_owned();
                let is_compact = headers
                    .get("x-dcl-request-kind")
                    .and_then(|v| v.to_str().ok())
                    == Some("compaction")
                    || path.ends_with("/responses/compact");
                let is_stream = body["stream"] == true;
                recorded.lock().unwrap().push((path.clone(), headers, body));

                if path.ends_with("/responses/compact") {
                    return (
                        StatusCode::OK,
                        Json(json!({
                            "object": "response.compaction",
                            "output": [
                                {"role":"user","content":[{"type":"input_text","text":"Inspect evidence.txt and summarize findings"}]},
                                {"type":"compaction","id":"cmp-1","encrypted_content":"opaque-mid-turn-summary"}
                            ],
                            "usage": {"input_tokens": 9000, "output_tokens": 120}
                        })),
                    )
                        .into_response();
                }

                if is_compact {
                    let summary = "MID_TURN_SUMMARY: Read evidence.txt (found BULKY_EVIDENCE_MARKER); next step is to report final conclusion.";
                    if !is_stream {
                        return match protocol {
                            Protocol::Anthropic => Json(json!({
                                "id": "msg_frag",
                                "type": "message",
                                "role": "assistant",
                                "content": [{"type": "text", "text": summary}],
                                "stop_reason": "end_turn",
                                "usage": {"input_tokens": 1000, "output_tokens": 20}
                            }))
                            .into_response(),
                            _ => Json(json!({
                                "id": "frag",
                                "object": "chat.completion",
                                "created": 0,
                                "model": "fixture-model",
                                "choices": [{"index": 0, "message": {"role": "assistant", "content": summary}, "finish_reason": "stop"}],
                                "usage": {"prompt_tokens": 1000, "completion_tokens": 20, "total_tokens": 1020}
                            }))
                            .into_response(),
                        };
                    }
                    return match protocol {
                        Protocol::Anthropic => (
                            [("content-type", "text/event-stream")],
                            anthropic_text_sse(summary, 9000),
                        )
                            .into_response(),
                        Protocol::OpenAIChat => (
                            [("content-type", "text/event-stream")],
                            openai_sse(json!({"content": summary}), "stop", 9000),
                        )
                            .into_response(),
                        Protocol::OpenAIResponses => (
                            [("content-type", "text/event-stream")],
                            responses_sse(
                                json!([{"type":"message","role":"assistant","content":[{"type":"output_text","text":summary}]}]),
                                Some(summary),
                                9000,
                            ),
                        )
                            .into_response(),
                    };
                }

                let step = main_step.fetch_add(1, Ordering::SeqCst);
                let read_args = json!({"file_path": file_path});
                match protocol {
                    Protocol::OpenAIChat => {
                        if step == 0 {
                            let delta = json!({
                                "tool_calls": [{
                                    "index": 0,
                                    "id": "call-read-1",
                                    "type": "function",
                                    "function": {"name": "Read", "arguments": read_args.to_string()}
                                }]
                            });
                            (
                                [("content-type", "text/event-stream")],
                                openai_sse(delta, "tool_calls", 300),
                            )
                                .into_response()
                        } else {
                            (
                                [("content-type", "text/event-stream")],
                                openai_sse(
                                    json!({"content": "FINAL_ANSWER_AFTER_MID_TURN_COMPACT"}),
                                    "stop",
                                    400,
                                ),
                            )
                                .into_response()
                        }
                    }
                    Protocol::Anthropic => {
                        if step == 0 {
                            (
                                [("content-type", "text/event-stream")],
                                anthropic_tool_sse("toolu_read_1", "Read", &read_args, 300),
                            )
                                .into_response()
                        } else {
                            (
                                [("content-type", "text/event-stream")],
                                anthropic_text_sse("FINAL_ANSWER_AFTER_MID_TURN_COMPACT", 400),
                            )
                                .into_response()
                        }
                    }
                    Protocol::OpenAIResponses => {
                        if step == 0 {
                            let output = json!([{
                                "type": "function_call",
                                "id": "fc-read-1",
                                "call_id": "call-read-1",
                                "name": "Read",
                                "arguments": read_args.to_string()
                            }]);
                            (
                                [("content-type", "text/event-stream")],
                                responses_sse(output, None, 300),
                            )
                                .into_response()
                        } else {
                            let output = json!([{
                                "type": "message",
                                "role": "assistant",
                                "content": [{"type": "output_text", "text": "FINAL_ANSWER_AFTER_MID_TURN_COMPACT"}]
                            }]);
                            (
                                [("content-type", "text/event-stream")],
                                responses_sse(
                                    output,
                                    Some("FINAL_ANSWER_AFTER_MID_TURN_COMPACT"),
                                    400,
                                ),
                            )
                                .into_response()
                        }
                    }
                }
            }
        },
    ));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let (api_format, builtin_id, model_api, base_url) = match protocol {
        Protocol::OpenAIChat => ("openai", None, None, format!("http://{addr}/v1")),
        Protocol::Anthropic => ("anthropic", None, None, format!("http://{addr}")),
        Protocol::OpenAIResponses => (
            "openai",
            Some("openai"),
            Some("openai-responses"),
            format!("http://{addr}/tools/codex/v1"),
        ),
    };
    let provider: Provider = serde_json::from_value(json!({
        "id": "fixture",
        "name": "Fixture",
        "builtin_id": builtin_id,
        "api_format": api_format,
        "base_url": base_url,
        "api_key": "fixture-key",
        "enabled": true,
        "models": [{
            "id": model_id,
            "name": model_id,
            "api": model_api,
            "enabled": true,
            "max_tokens": 1000,
            "context_window": 32000,
            "supports_web_search": false
        }]
    }))
    .unwrap();

    let permissions = runtime::default_permissions();
    permissions.set_mode(PermissionMode::BypassPermissions);
    let engine = runtime::create_engine(
        &paths,
        store.db.clone(),
        Arc::new(tokio::sync::Mutex::new(ProviderManager::in_memory(
            vec![provider.clone()],
            paths.data_dir.join("config/providers.json"),
        ))),
        permissions,
        Arc::new(FileCache::new()),
    );

    let format = match protocol {
        Protocol::Anthropic => ApiFormat::Anthropic,
        _ => ApiFormat::OpenAI,
    };
    let request: ChatRequest = serde_json::from_value(json!({
        "conversation_id": id,
        "messages": store.model_history(&id, &format).unwrap(),
        "assistant_message_id": assistant_id,
        "standalone_frontend": standalone_frontend,
        "model": model_id,
        "provider_id": "fixture",
        "workspace_path": dir.path(),
        "max_tokens": 1000
    }))
    .unwrap();

    let mut rx = engine
        .send_message_with_history(request, false)
        .await
        .unwrap();
    let mut events = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        while let Some(event) = rx.recv().await {
            assert!(!matches!(event, EngineEvent::Error(_)), "unexpected error: {event:?}");
            events.push(event);
        }
    })
    .await
    .unwrap();

    server.abort();
    let _ = server.await;
    let calls = recorded.lock().unwrap().clone();
    (dir, store, id, calls, events)
}

#[tokio::test]
async fn openai_mid_turn_tool_loop_triggers_auto_compact_and_continues() {
    let prefs = Preferences {
        autocompact_percent: Some(50.try_into().unwrap()),
        ..Default::default()
    };
    let (dir, store, id, calls, events) =
        run_mid_turn_scenario(Protocol::OpenAIChat, prefs, true).await;

    // 1) Initial main request -> 2) mid-turn compaction request -> 3) resumed main request.
    assert_eq!(calls.len(), 3, "expected main + compact + main calls");
    assert_eq!(calls[1].1["x-dcl-request-kind"], "compaction");
    let resumed_body = calls[2].2.to_string();
    assert!(resumed_body.contains("MID_TURN_SUMMARY"));
    assert!(!resumed_body.contains("BULKY_EVIDENCE_MARKER_0123456789"));

    // Emitted UI notices & ContextReset(AutoCompact) with rolled-over segment.
    assert!(events.iter().any(|e| matches!(
        e,
        EngineEvent::CompactionNotice(msg) if msg.contains("Auto-compacting conversation")
    )));
    let reset = events.iter().find_map(|e| match e {
        EngineEvent::ContextReset {
            boundary_id,
            assistant_id,
            reason: ContextResetReason::AutoCompact { notice },
            ..
        } => Some((boundary_id.clone(), assistant_id.clone(), notice.clone())),
        _ => None,
    });
    let (boundary_id, next_assistant, notice) = reset.expect("expected AutoCompact ContextReset");
    assert!(next_assistant.is_some());
    assert!(notice.contains("Compacted in"), "{notice}");
    assert!(notice.contains("Summary saved to:"), "{notice}");

    // Exported summary file exists in cache/compactions/<session>/.
    let export_dir = dir.path().join("data/cache/compactions").join(&id);
    let md_files: Vec<_> = std::fs::read_dir(&export_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("md"))
        .collect();
    assert_eq!(md_files.len(), 1);

    // Durable history has the boundary row and the rolled-over assistant segment after it.
    let rows = store
        .db
        .open_session(&id)
        .unwrap()
        .repository()
        .messages()
        .unwrap();
    let boundary_pos = rows
        .iter()
        .position(|m| m.id == boundary_id && m.is_compact_boundary)
        .expect("expected persisted compact boundary row");
    let next_assistant_id = next_assistant.unwrap();
    let rolled_pos = rows
        .iter()
        .position(|m| m.id == next_assistant_id)
        .expect("expected rolled-over assistant segment row");
    assert!(rolled_pos > boundary_pos);
    assert_eq!(
        deepseek_code_core::services::compaction::count(&store.db, &id)
            .unwrap()
            .proven(),
        1
    );
    assert!(events.iter().any(|e| matches!(
        e,
        EngineEvent::MessageStop { full_text, .. } if full_text.contains("FINAL_ANSWER_AFTER_MID_TURN_COMPACT")
    )));
}

#[tokio::test]
async fn anthropic_mid_turn_tool_loop_triggers_auto_compact_and_continues() {
    let prefs = Preferences {
        autocompact_percent: Some(50.try_into().unwrap()),
        ..Default::default()
    };
    let (_dir, _store, _id, calls, events) =
        run_mid_turn_scenario(Protocol::Anthropic, prefs, true).await;

    assert_eq!(calls.len(), 3, "expected main + compact + main calls");
    assert_eq!(calls[1].1["x-dcl-request-kind"], "compaction");
    let resumed_messages = calls[2].2["messages"].as_array().unwrap();
    // Valid Anthropic alternation: single user message containing the summary + continuation cue.
    assert_eq!(resumed_messages.len(), 1);
    assert_eq!(resumed_messages[0]["role"], "user");
    let resumed_text = resumed_messages[0].to_string();
    assert!(resumed_text.contains("MID_TURN_SUMMARY"));
    assert!(resumed_text.contains("Continue executing the current task immediately"));
    assert!(!resumed_text.contains("BULKY_EVIDENCE_MARKER_0123456789"));

    assert!(events.iter().any(|e| matches!(
        e,
        EngineEvent::ContextReset {
            reason: ContextResetReason::AutoCompact { .. },
            ..
        }
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        EngineEvent::MessageStop { full_text, .. } if full_text.contains("FINAL_ANSWER_AFTER_MID_TURN_COMPACT")
    )));
}

#[tokio::test]
async fn native_responses_mid_turn_auto_compact_uses_responses_compact_and_continues() {
    let prefs = Preferences {
        autocompact_percent: Some(50.try_into().unwrap()),
        ..Default::default()
    };
    let (_dir, _store, _id, calls, events) =
        run_mid_turn_scenario(Protocol::OpenAIResponses, prefs, false).await;

    assert_eq!(calls.len(), 3, "expected responses + /responses/compact + responses");
    assert!(calls[1].0.ends_with("/responses/compact"));
    let resumed_input = calls[2].2["input"].as_array().unwrap();
    assert!(resumed_input
        .iter()
        .any(|item| item["type"] == "compaction" && item["encrypted_content"] == "opaque-mid-turn-summary"));
    assert!(resumed_input
        .iter()
        .any(|item| item.to_string().contains("Continue executing the current task immediately")));
    assert!(!calls[2].2.to_string().contains("BULKY_EVIDENCE_MARKER_0123456789"));

    assert!(events.iter().any(|e| matches!(
        e,
        EngineEvent::ContextReset {
            reason: ContextResetReason::AutoCompact { .. },
            ..
        }
    )));
}

#[tokio::test]
async fn autocompact_off_disables_mid_turn_auto_compact() {
    let prefs = Preferences {
        autocompact_off: true,
        ..Default::default()
    };
    let (_dir, store, id, calls, events) =
        run_mid_turn_scenario(Protocol::OpenAIChat, prefs, true).await;

    // Only the two main requests should run — no compaction request, no ContextReset.
    assert_eq!(calls.len(), 2, "expected only 2 main calls when autocompact is off");
    assert!(calls.iter().all(|(_, headers, _)| {
        headers
            .get("x-dcl-request-kind")
            .and_then(|v| v.to_str().ok())
            != Some("compaction")
    }));
    assert!(!events
        .iter()
        .any(|e| matches!(e, EngineEvent::ContextReset { .. })));
    assert_eq!(
        deepseek_code_core::services::compaction::count(&store.db, &id)
            .unwrap()
            .proven(),
        0
    );
}

#[tokio::test]
async fn anthropic_mid_turn_microcompact_clears_old_tool_results_before_llm_compact() {
    deepseek_code_core::config::OrchestrationConfig::init_global().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let big_file = dir.path().join("big.txt");
    let small_file = dir.path().join("small.txt");
    // ~45 KB (~15,000 tokens) read in slot 0, followed by 10 tiny reads so microcompact
    // clears slot 0 while keeping the last 10 tool_result blocks, dropping well below 16,000.
    std::fs::write(&big_file, "OLD_LARGE_RESULT_TOKEN ".repeat(2000)).unwrap();
    std::fs::write(&small_file, "tiny").unwrap();

    let paths = RuntimePaths::new(dir.path().join("data")).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let id = store.create(dir.path(), "fixture-model", "fixture").unwrap();
    // 50% of 32,000 = 16,000 tokens threshold. After 11 reads, slot 0 is older than KEEP_RECENT (10),
    // so microcompact clears slot 0 and drops total well below 16,000 — avoiding an LLM compaction!
    store
        .db
        .open_session(&id)
        .unwrap()
        .write(|w| {
            w.set_setting(
                PREFERENCES,
                &Preferences {
                    autocompact_percent: Some(50.try_into().unwrap()),
                    ..Default::default()
                },
            )
        })
        .unwrap();
    store.append(&id, "user", "Read big then small files").unwrap();
    let mut turn = store.db.start_turn_timer(&id).unwrap();
    let assistant_id = turn.ensure_segment().unwrap();

    let recorded: Arc<Mutex<Vec<(HeaderMap, Value)>>> = Arc::default();
    let recorded_clone = recorded.clone();
    let step = Arc::new(AtomicUsize::new(0));
    let step_clone = step.clone();
    let big_path = big_file.to_string_lossy().into_owned();
    let small_path = small_file.to_string_lossy().into_owned();

    let app = Router::new().fallback(post(move |headers: HeaderMap, Json(body): Json<Value>| {
        let recorded = recorded_clone.clone();
        let step = step_clone.clone();
        let big_path = big_path.clone();
        let small_path = small_path.clone();
        async move {
            recorded.lock().unwrap().push((headers, body));
            let s = step.fetch_add(1, Ordering::SeqCst);
            if s == 0 {
                // Batch 11 reads in one assistant turn: 1 big + 10 small.
                let mut events = vec![
                    json!({"type":"message_start","message":{"id":"m","usage":{"input_tokens":200}}}),
                ];
                for i in 0..11 {
                    let path = if i == 0 { &big_path } else { &small_path };
                    let input = json!({"file_path": path});
                    events.push(json!({"type":"content_block_start","index":i,"content_block":{"type":"tool_use","id":format!("toolu_{i}"),"name":"Read","input":{}}}));
                    events.push(json!({"type":"content_block_delta","index":i,"delta":{"type":"input_json_delta","partial_json":input.to_string()}}));
                    events.push(json!({"type":"content_block_stop","index":i}));
                }
                events.push(json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":20}}));
                events.push(json!({"type":"message_stop"}));
                let frames: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
                ([("content-type", "text/event-stream")], frames).into_response()
            } else {
                (
                    [("content-type", "text/event-stream")],
                    anthropic_text_sse("DONE_VIA_MICROCOMPACT", 300),
                )
                    .into_response()
            }
        }
    }));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let provider: Provider = serde_json::from_value(json!({
        "id": "fixture",
        "name": "Fixture",
        "api_format": "anthropic",
        "base_url": format!("http://{addr}"),
        "api_key": "fixture-key",
        "enabled": true,
        "models": [{
            "id": "fixture-model",
            "name": "fixture-model",
            "enabled": true,
            "max_tokens": 1000,
            "context_window": 32000,
            "supports_web_search": false
        }]
    }))
    .unwrap();
    let permissions = runtime::default_permissions();
    permissions.set_mode(PermissionMode::BypassPermissions);
    let engine = runtime::create_engine(
        &paths,
        store.db.clone(),
        Arc::new(tokio::sync::Mutex::new(ProviderManager::in_memory(
            vec![provider],
            paths.data_dir.join("config/providers.json"),
        ))),
        permissions,
        Arc::new(FileCache::new()),
    );
    let request: ChatRequest = serde_json::from_value(json!({
        "conversation_id": id,
        "messages": store.model_history(&id, &ApiFormat::Anthropic).unwrap(),
        "assistant_message_id": assistant_id,
        "standalone_frontend": true,
        "model": "fixture-model",
        "provider_id": "fixture",
        "workspace_path": dir.path(),
        "max_tokens": 1000
    }))
    .unwrap();

    let mut rx = engine
        .send_message_with_history(request, false)
        .await
        .unwrap();
    let mut events = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        while let Some(event) = rx.recv().await {
            assert!(!matches!(event, EngineEvent::Error(_)), "{event:?}");
            events.push(event);
        }
    })
    .await
    .unwrap();

    server.abort();
    let _ = server.await;
    let calls = recorded.lock().unwrap();
    // Microcompact cleared slot 0 and stayed below threshold -> only 2 main requests, NO LLM compaction call!
    assert_eq!(calls.len(), 2);
    assert!(calls.iter().all(|(headers, _)| {
        headers
            .get("x-dcl-request-kind")
            .and_then(|v| v.to_str().ok())
            != Some("compaction")
    }));
    let second_body = calls[1].1.to_string();
    assert!(second_body.contains("[Old tool result content cleared to save context]"));
    assert!(!second_body.contains("OLD_LARGE_RESULT_TOKEN"));
    assert!(events.iter().any(|e| matches!(
        e,
        EngineEvent::CompactionNotice(msg) if msg.contains("Cleared older tool results")
    )));
    assert!(!events
        .iter()
        .any(|e| matches!(e, EngineEvent::ContextReset { .. })));
}

#[tokio::test]
async fn mid_turn_compaction_cancellation_leaves_db_unmodified() {
    deepseek_code_core::config::OrchestrationConfig::init_global().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let big_file = dir.path().join("evidence.txt");
    std::fs::write(&big_file, "CANCEL_EVIDENCE_DATA ".repeat(2400)).unwrap();

    let paths = RuntimePaths::new(dir.path().join("data")).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let id = store.create(dir.path(), "fixture-model", "fixture").unwrap();
    store
        .db
        .open_session(&id)
        .unwrap()
        .write(|w| {
            w.set_setting(
                PREFERENCES,
                &Preferences {
                    autocompact_percent: Some(50.try_into().unwrap()),
                    ..Default::default()
                },
            )
        })
        .unwrap();
    store.append(&id, "user", "Read evidence").unwrap();
    let mut turn = store.db.start_turn_timer(&id).unwrap();
    let assistant_id = turn.ensure_segment().unwrap();

    let compact_started = Arc::new(Notify::new());
    let compact_started_clone = compact_started.clone();
    let file_path = big_file.to_string_lossy().into_owned();

    let app = Router::new().fallback(post(move |headers: HeaderMap, Json(_body): Json<Value>| {
        let compact_started = compact_started_clone.clone();
        let file_path = file_path.clone();
        async move {
            let is_compact = headers
                .get("x-dcl-request-kind")
                .and_then(|v| v.to_str().ok())
                == Some("compaction");
            if is_compact {
                compact_started.notify_one();
                std::future::pending::<()>().await;
            }
            let delta = json!({
                "tool_calls": [{
                    "index": 0,
                    "id": "call-read-1",
                    "type": "function",
                    "function": {"name": "Read", "arguments": json!({"file_path": file_path}).to_string()}
                }]
            });
            (
                [("content-type", "text/event-stream")],
                openai_sse(delta, "tool_calls", 300),
            )
                .into_response()
        }
    }));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let provider: Provider = serde_json::from_value(json!({
        "id": "fixture",
        "name": "Fixture",
        "api_format": "openai",
        "base_url": format!("http://{addr}/v1"),
        "api_key": "fixture-key",
        "enabled": true,
        "models": [{
            "id": "fixture-model",
            "name": "fixture-model",
            "enabled": true,
            "max_tokens": 1000,
            "context_window": 32000,
            "supports_web_search": false
        }]
    }))
    .unwrap();
    let permissions = runtime::default_permissions();
    permissions.set_mode(PermissionMode::BypassPermissions);
    let engine = runtime::create_engine(
        &paths,
        store.db.clone(),
        Arc::new(tokio::sync::Mutex::new(ProviderManager::in_memory(
            vec![provider],
            paths.data_dir.join("config/providers.json"),
        ))),
        permissions,
        Arc::new(FileCache::new()),
    );
    let request: ChatRequest = serde_json::from_value(json!({
        "conversation_id": id,
        "messages": store.model_history(&id, &ApiFormat::OpenAI).unwrap(),
        "assistant_message_id": assistant_id,
        "standalone_frontend": true,
        "model": "fixture-model",
        "provider_id": "fixture",
        "workspace_path": dir.path(),
        "max_tokens": 1000
    }))
    .unwrap();

    let rx = engine
        .send_message_with_history(request, false)
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), compact_started.notified())
        .await
        .expect("mid-turn compaction should have started");
    // Dropping the receiver simulates Ctrl-C / host cancellation while compaction is in flight.
    drop(rx);
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    server.abort();
    let _ = server.await;
    assert_eq!(
        deepseek_code_core::services::compaction::count(&store.db, &id)
            .unwrap()
            .proven(),
        0,
        "cancelled mid-turn compaction must not commit a boundary"
    );
}
