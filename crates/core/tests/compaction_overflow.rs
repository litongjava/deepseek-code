use axum::{routing::post, Json, Router};
use deepseek_code_core::{
    cache::FileCache,
    native_engine::{
        provider_manager::{Provider, ProviderManager},
        NativeEngine,
    },
    runtime::{self, RuntimePaths, SessionStore},
    services::compaction,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

async fn fixture(
    format: &str,
    mode: u8,
    real_data: bool,
) -> (
    tempfile::TempDir,
    SessionStore,
    String,
    NativeEngine,
    Arc<Mutex<Vec<Value>>>,
    tokio::task::JoinHandle<()>,
) {
    let dir = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(dir.path().join("data")).unwrap();
    if real_data {
        std::fs::create_dir_all(&paths.data_dir).unwrap();
        std::fs::copy(
            std::env::var_os("DCL_COMPACTION_BACKUP").expect("Set a read-only backup path"),
            paths.data_dir.join("deepseek_code.db"),
        )
        .unwrap();
    }
    let store = SessionStore::new(paths.open_database().unwrap());
    let id = if real_data {
        std::env::var("DCL_COMPACTION_SESSION").expect("Set the backed-up session ID")
    } else {
        store.create(dir.path(), "gpt-6-astra", "fixture").unwrap()
    };
    if !real_data {
        store
            .append(
                &id,
                "user",
                "Original request: finish the remaining tests; preserve 中文 paths.",
            )
            .unwrap();
        store
            .append(
                &id,
                "assistant",
                &format!(
                    "{}\nLatest unresolved error: NEEDS_TESTING",
                    "Tool evidence 中文🙂\n".repeat(30_000)
                ),
            )
            .unwrap();
    }
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();
    let anthropic = format == "anthropic";
    let window = if real_data { 256_000 } else { 16_000 };
    let app = Router::new().route(if anthropic { "/v1/messages" } else { "/v1/chat/completions" }, post(move |Json(body): Json<Value>| {
        let captured = captured.clone();
        async move {
            assert!(body.get("tools").is_none_or(|v| v.as_array().is_some_and(Vec::is_empty)));
            let usage = deepseek_code_core::native_engine::request_context::ContextUsage::from_body(if anthropic { "anthropic-messages" } else { "openai-completions" }, &body, Some(window), 4096);
            assert!(usage.text_subtotal() + 4096 <= u64::from(window) * 9 / 10, "wire input {} plus output exceeds safe window {}", usage.text_subtotal(), u64::from(window) * 9 / 10);
            let number = { let mut requests = captured.lock().unwrap(); requests.push(body); requests.len() };
            if mode == 2 && number == 2 { std::future::pending::<()>().await; }
            let fail = mode == 1 && number == 2;
            let text = "Original request retained; NEEDS_TESTING: finish the remaining tests.";
            Json(if anthropic {
                json!({"id":"r","type":"message","role":"assistant","model":"gpt-6-astra","content":[{"type":"text","text":text}],"stop_reason":if fail { "max_tokens" } else { "end_turn" },"usage":{"input_tokens":1,"output_tokens":1}})
            } else {
                json!({"id":"r","object":"chat.completion","created":0,"model":"gpt-6-astra","choices":[{"index":0,"message":{"role":"assistant","content":text},"finish_reason":if fail { "length" } else { "stop" }}]})
            })
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider: Provider = serde_json::from_value(json!({"id":"fixture","name":"fixture","base_url":format!("http://{}",listener.local_addr().unwrap()),"api_key":"fixture","api_format":format,"enabled":true,"models":[{"id":"gpt-6-astra","name":"fixture","enabled":true,"supports_vision":false,"supports_web_search":false,"context_window":window,"max_tokens":4096}]})).unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let engine = runtime::create_engine(
        &paths,
        store.db.clone(),
        Arc::new(tokio::sync::Mutex::new(ProviderManager::in_memory(
            vec![provider],
            paths.data_dir.join("providers.json"),
        ))),
        runtime::default_permissions(),
        Arc::new(FileCache::new()),
    );
    (dir, store, id, engine, requests, server)
}

#[tokio::test]
async fn oversized_compaction_is_bounded_and_persists_only_the_final_summary() {
    for format in ["openai", "anthropic"] {
        let (dir, store, id, engine, requests, server) = fixture(format, 0, false).await;
        let before = store.history(&id).unwrap();
        let report = compaction::compact(
            &engine,
            &store.db,
            &id,
            "fixture",
            "gpt-6-astra",
            "retain pending tests",
        )
        .await
        .unwrap();
        assert_eq!(report.summarized_messages, 2);
        let after = store.history(&id).unwrap();
        assert_eq!(&after[..before.len()], before.as_slice());
        assert_eq!(after.len(), before.len() + 1);
        assert!(requests.lock().unwrap().len() > 2);
        let reopened = SessionStore::new(
            RuntimePaths::new(dir.path().join("data"))
                .unwrap()
                .open_database()
                .unwrap(),
        );
        for protocol in [
            deepseek_code_core::native_engine::provider_manager::ApiFormat::OpenAI,
            deepseek_code_core::native_engine::provider_manager::ApiFormat::Anthropic,
        ] {
            let active = reopened.model_history(&id, &protocol).unwrap();
            assert_eq!(active.len(), 1);
            assert!(active[0]["content"]
                .as_str()
                .unwrap()
                .contains("NEEDS_TESTING"));
        }
        server.abort();
    }
}

#[tokio::test]
async fn compaction_fragment_failure_and_cancellation_keep_original_history() {
    for format in ["openai", "anthropic"] {
        for mode in [1, 2] {
            let (_dir, store, id, engine, requests, server) = fixture(format, mode, false).await;
            let before = store.history(&id).unwrap();
            let future = compaction::compact(&engine, &store.db, &id, "fixture", "gpt-6-astra", "");
            if mode == 1 {
                let error = future.await.unwrap_err();
                assert!(format!("{error:#}").contains("Compaction fragment 2/"));
                assert!(format!("{error:#}").contains("Generation did not finish normally"));
            } else {
                // Drop compaction while the second HTTP request is pending.
                tokio::time::timeout(std::time::Duration::from_secs(30), async {
                    tokio::pin!(future);
                    tokio::select! {
                        _ = &mut future => panic!("Summary must remain pending"),
                        _ = async {
                            while requests.lock().unwrap().len() < 2 {
                                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                            }
                        } => {}
                    }
                })
                .await
                .unwrap();
            }
            assert_eq!(requests.lock().unwrap().len(), 2);
            assert_eq!(store.history(&id).unwrap(), before);
            server.abort();
        }
    }
}

#[tokio::test]
async fn generation_uses_token_estimates_but_still_rejects_over_budget_requests() {
    let (_dir, _store, _id, engine, requests, server) = fixture("openai", 0, false).await;
    let input = "中文 text ".repeat(2000);
    assert!(input.len() > 16_000);
    engine
        .generate_text("fixture", "gpt-6-astra", "Summarize", &input)
        .await
        .unwrap();
    assert_eq!(requests.lock().unwrap().len(), 1);
    let error = engine
        .generate_text("fixture", "gpt-6-astra", "Summarize", &input.repeat(10))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("configured context budget"));
    assert_eq!(requests.lock().unwrap().len(), 1);
    server.abort();
}

#[tokio::test]
async fn oversized_rewind_summary_keeps_original_and_creates_a_resumable_branch() {
    let (_dir, store, id, engine, requests, server) = fixture("openai", 0, false).await;
    let before = store.history(&id).unwrap();
    let message_id = store
        .db
        .with_conn(|c| {
            c.query_row(
                "SELECT id FROM messages WHERE conversation_id=?1 ORDER BY sort_order DESC LIMIT 1",
                [&id],
                |r| r.get::<_, String>(0),
            )
        })
        .unwrap()
        .unwrap();
    let (branch, count) = compaction::summarize_part(
        &engine,
        &store.db,
        &id,
        &message_id,
        compaction::Part::From,
        "fixture",
        "gpt-6-astra",
    )
    .await
    .unwrap();
    assert_eq!(count, 1);
    assert_eq!(store.history(&id).unwrap(), before);
    assert_eq!(store.history(&branch).unwrap().len(), 2);
    assert!(requests.lock().unwrap().len() > 2);
    server.abort();
}

#[tokio::test]
#[ignore = "requires an explicitly supplied real-data backup; only localhost fixture requests"]
async fn real_session_compaction_replay_uses_copied_database() {
    for format in ["openai", "anthropic"] {
        let (dir, store, id, engine, requests, server) = fixture(format, 0, true).await;
        let before = store.history(&id).unwrap();
        let before_tools = store.db.with_conn(|c| c.query_row("SELECT COUNT(*) FROM tool_calls WHERE message_id IN (SELECT id FROM messages WHERE conversation_id=?1)", [&id], |r| r.get::<_, i64>(0))).unwrap().unwrap();
        let report = compaction::compact(
            &engine,
            &store.db,
            &id,
            "fixture",
            "gpt-6-astra",
            "retain pending tests",
        )
        .await
        .unwrap();
        assert_eq!(report.summarized_messages, before.len());
        assert_eq!(
            &store.history(&id).unwrap()[..before.len()],
            before.as_slice()
        );
        let calls = requests.lock().unwrap().len();
        assert!(calls > 2);
        let reopened = SessionStore::new(
            RuntimePaths::new(dir.path().join("data"))
                .unwrap()
                .open_database()
                .unwrap(),
        );
        let active = reopened
            .model_history(
                &id,
                &deepseek_code_core::native_engine::provider_manager::ApiFormat::OpenAI,
            )
            .unwrap();
        assert_eq!(active.len(), 1);
        let after_tools = store.db.with_conn(|c| c.query_row("SELECT COUNT(*) FROM tool_calls WHERE message_id IN (SELECT id FROM messages WHERE conversation_id=?1)", [&id], |r| r.get::<_, i64>(0))).unwrap().unwrap();
        assert_eq!(before_tools, after_tools);
        println!("{format}: {} original messages and {before_tools} tools preserved; {calls} bounded local summary requests; resumed history {} message", before.len(), active.len());
        server.abort();
    }
}
