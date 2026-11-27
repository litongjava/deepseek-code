//! Manual compaction after a completed main turn, with recording disabled.
#[path = "support/session_sql.rs"]
mod session_sql;
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
use session_sql::FixtureSql;
use std::sync::{Arc, Mutex};

async fn scenario(api: &str, truncate: bool) {
    deepseek_code_core::config::OrchestrationConfig::init_global().unwrap();
    let root = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(root.path().join("data")).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let id = store.create(root.path(), "model", "fixture").unwrap();
    store.append(&id, "user", "first user input").unwrap();
    store.append(&id, "assistant", "first reply").unwrap();
    store.append(&id, "user", "second question").unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let seen = calls.clone();
    let anthropic = api == "anthropic";
    let responses = api == "openai-responses";
    let app = Router::new().fallback(post(move |headers: HeaderMap, Json(body): Json<Value>| {
        let compact = headers["x-dcl-request-kind"] == "compaction";
        let fragment = body["stream"] != true;
        seen.lock().unwrap().push((headers,body));
        async move {
            let text = if compact { "Verified summary." } else { "last answer" };
            if fragment {
                assert!(compact && truncate);
                return Json(json!({"id":"fragment","object":"chat.completion","created":0,"model":"model",
                    "choices":[{"index":0,"message":{"role":"assistant","content":text},"finish_reason":"stop"}],
                    "usage":{"prompt_tokens":41,"completion_tokens":5,"total_tokens":46,"prompt_cache_hit_tokens":11}})).into_response();
            }
            let input = json!({"file_path":"should-never-exist","content":"executed"});
            let cached = if compact { 80 } else { 0 };
            let events = if responses {
                let mut output = vec![json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]})];
                if compact {
                    output.push(json!({"type":"function_call","id":"fc-discard","call_id":"discard-me","name":"Write","arguments":input.to_string()}));
                }
                vec![json!({"type":"response.output_text.delta","delta":text}),
                    json!({"type":"response.completed","response":{"id":"response","status":"completed","output":output,
                    "usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":cached},"output_tokens":3,"total_tokens":103}}})]
            } else if anthropic {
                let mut events = vec![
                    json!({"type":"message_start","message":{"id":"response","usage":{"input_tokens":20,"cache_read_input_tokens":cached}}}),
                    json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                    json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
                    json!({"type":"content_block_stop","index":0}),
                ];
                if compact {
                    events.push(json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"discard-me","name":"Write","input":{}}}));
                    events.push(json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":input.to_string()}}));
                    events.push(json!({"type":"content_block_stop","index":1}));
                }
                events.push(json!({"type":"message_delta","delta":{"stop_reason":if compact {"tool_use"} else {"end_turn"}},"usage":{"output_tokens":3}}));
                events.push(json!({"type":"message_stop"}));
                events
            } else {
                let mut delta = json!({"content":if compact && truncate { "DISCARD_PARTIAL" } else { text }});
                if compact {
                    delta["tool_calls"] = json!([{"index":0,"id":"discard-me","type":"function","function":{"name":"Write","arguments":input.to_string()}}]);
                }
                vec![json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
                    json!({"choices":[{"index":0,"delta":{},"finish_reason":if compact && truncate {"length"} else if compact {"tool_calls"} else {"stop"}}],
                        "usage":{"prompt_tokens":100,"completion_tokens":3,"total_tokens":103,"prompt_tokens_details":{"cached_tokens":cached}}})]
            };
            let mut frames = events.iter().map(|v| format!("data: {v}\n\n")).collect::<String>();
            if !anthropic && !responses {frames.push_str("data: [DONE]\n\n");}
            ([("content-type","text/event-stream")], frames).into_response()
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let provider: Provider = serde_json::from_value(json!({"id":"fixture","name":"Fixture","base_url":format!("http://{address}"),
        "api_key":"fixture","api_format":if responses {"openai"} else {api},"builtin_id":responses.then_some("openai"),"enabled":true,"models":[{"id":"model","name":"model","enabled":true,
        "api":responses.then_some("openai-responses"),"max_tokens":4096,"context_window":128000,"supports_web_search":false}]})).unwrap();
    let engine = || {
        runtime::create_engine(
            &paths,
            store.db.clone(),
            Arc::new(tokio::sync::Mutex::new(ProviderManager::in_memory(
                vec![provider.clone()],
                paths.data_dir.join("config/providers.json"),
            ))),
            runtime::default_permissions(),
            Arc::new(FileCache::new()),
        )
    };
    store.append(&id, "assistant", "last answer").unwrap();
    // A user can append input before asking to compact. The whole request fits
    // the 128K window even when this suffix exceeds the fragment planner's 64K.
    let long_suffix = (api == "openai" && !truncate).then(|| "中".repeat(70_000));
    if let Some(text) = &long_suffix {
        store.append(&id, "user", text).unwrap();
    }
    let main = engine();
    let request: ChatRequest = serde_json::from_value(json!({"conversation_id":id,
        "messages":store.model_history(&id,&provider.api_format).unwrap(),
        "system_prompt":"Custom session instructions","model":"model","provider_id":"fixture","workspace_path":root.path()})).unwrap();
    let mut events = main
        .send_message_with_history(request, false)
        .await
        .unwrap();
    while let Some(event) = events.recv().await {
        assert!(
            !matches!(
                event,
                deepseek_code_core::native_engine::tool_loop::EngineEvent::Error(_)
            ),
            "{event:?}"
        );
    }
    let shard = store.db.open_session(&id).unwrap();
    let input_before = shard.repository().usage_totals(&[]).unwrap().1;
    let before = store.history(&id).unwrap();
    compaction::compact(
        &engine(),
        &store.db,
        &id,
        "fixture",
        "model",
        "Keep pending work",
    )
    .await
    .unwrap();
    server.abort();
    let _ = server.await;
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), if truncate { 5 } else { 2 });
    let mut body = calls[1].1.clone();
    let messages = body[if responses { "input" } else { "messages" }]
        .as_array_mut()
        .unwrap();
    let instruction = messages.pop().unwrap();
    assert_eq!(instruction["role"], "user");
    assert!(instruction["content"]
        .to_string()
        .contains("Keep pending work"));
    if let Some(text) = long_suffix {
        let extra = messages.last().unwrap();
        assert_eq!(extra["role"], "user");
        assert_eq!(extra["content"].as_str().unwrap(), text);
    }
    let last = messages
        .iter()
        .rev()
        .find(|m| m["role"] == "assistant")
        .unwrap();
    assert_eq!(last["role"], "assistant");
    assert!(last.to_string().contains("last answer"));
    assert!(messages
        .iter()
        .find(|m| m["role"] == "user")
        .unwrap()
        .to_string()
        .contains("system-reminder"));
    assert!(!body["tools"].as_array().unwrap().is_empty());
    assert_eq!(body, calls[0].1);
    assert_eq!(calls[1].0["x-dcl-request-kind"], "compaction");
    let rows = store.history(&id).unwrap();
    assert_eq!(&rows[..before.len()], before);
    assert_eq!(rows.len(), before.len() + 1);
    assert!(rows.last().unwrap()["content"]
        .as_str()
        .unwrap()
        .ends_with("Verified summary."));
    assert!(!root.path().join("should-never-exist").exists());
    let measured: (i64,i64,i64) = store.db.shard_sql(&id,|c| c.query_row(
        "SELECT input_tokens,cache_read_tokens,(SELECT count(*) FROM tool_calls) FROM model_requests WHERE id=?1",
        [calls[1].0["x-request-id"].to_str().unwrap()],|r| Ok((r.get(0)?,r.get(1)?,r.get(2)?)))).unwrap().unwrap();
    assert_eq!(measured, (100, 80, 0));
    let input_after = shard.repository().usage_totals(&[]).unwrap().1;
    assert_eq!(
        input_after - input_before,
        if truncate { 100 + 3 * 41 } else { 100 }
    );
    for (headers, body) in &calls[2..] {
        assert!(!body.to_string().contains("DISCARD_PARTIAL"));
        assert_eq!(body["max_tokens"], 4096);
        let counts: (i64, i64) = store
            .db
            .shard_sql(&id, |c| {
                c.query_row(
                    "SELECT input_tokens,cache_read_tokens FROM model_requests WHERE id=?1",
                    [headers["x-request-id"].to_str().unwrap()],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
            })
            .unwrap()
            .unwrap();
        assert_eq!(counts, (41, 11));
    }
}

#[tokio::test]
async fn openai_summary_reuses_the_main_request_and_discards_calls() {
    scenario("openai", false).await;
}

#[tokio::test]
async fn anthropic_summary_reuses_the_main_request_and_discards_calls() {
    scenario("anthropic", false).await;
}

#[tokio::test]
async fn non_gpt_responses_uses_the_main_input_and_discards_calls() {
    scenario("openai-responses", false).await;
}

#[tokio::test]
async fn truncated_prefix_splits_at_the_same_cap_and_records_each_recovery() {
    scenario("openai", true).await;
}
