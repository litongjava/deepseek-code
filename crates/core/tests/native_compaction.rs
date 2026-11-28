#[path = "support/session_sql.rs"]
mod session_sql;
use session_sql::FixtureSql;
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
        request_context::ContextUsage,
        token_counter::TokenCounter,
        NativeEngine,
    },
    runtime::{self, RuntimePaths, SessionStore},
    services::compaction,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

const PRESSURE_WINDOW: u32 = 96_000;
const PRESSURE_OUTPUT: u32 = 1_000;

const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jRZkAAAAASUVORK5CYII=";

fn window() -> Value {
    json!([
        {"role":"user","content":[{"type":"input_text","text":"Finish 中文 tests"}]},
        {"type":"reasoning","id":"reasoning-id","encrypted_content":"exact signed reasoning"},
        {"type":"compaction","id":"compact-id","encrypted_content":"opaque exact content"}
    ])
}

struct Fixture {
    dir: tempfile::TempDir,
    store: SessionStore,
    id: String,
    engine: NativeEngine,
    seen: Arc<Mutex<Vec<Value>>>,
    /// Set once the mock has validated and recorded a compaction request.
    arrived: Arc<Notify>,
    /// Mode 6 holds its 502 response until the test releases it.
    released: Arc<Notify>,
    server: tokio::task::JoinHandle<()>,
}

async fn fixture(mode: u8) -> Fixture {
    deepseek_code_core::config::OrchestrationConfig::init_global().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(dir.path().join("data")).unwrap();
    // ImageGenerate is not a default core tool; this fixture drives it directly.
    deepseek_code_core::services::customization::save(
        &paths.data_dir,
        &deepseek_code_core::services::customization::Customization {
            tool_loading: std::collections::BTreeMap::from([(
                "ImageGenerate".to_owned(),
                deepseek_code_core::services::customization::ToolLoading { enabled: true, on_demand: true },
            )]),
            ..Default::default()
        },
        &[],
    )
    .unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let id = store.create(dir.path(), "gpt-6-astra", "gateway").unwrap();
    store.append(&id, "user", "Finish 中文 tests").unwrap();
    store
        .append(&id, "assistant", "Tests remain pending")
        .unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let arrived = Arc::new(Notify::new());
    let released = Arc::new(Notify::new());
    let captured = seen.clone();
    let request_arrived = arrived.clone();
    let response_released = released.clone();
    let app = Router::new().route("/tools/codex/v1/responses/compact", post(move |headers: HeaderMap, Json(body): Json<Value>| {
        let seen = captured.clone();
        let arrived = request_arrived.clone();
        let released = response_released.clone();
        async move {
            assert_eq!(headers["authorization"], "Bearer gateway-key");
            for excluded in ["tools", "tool_choice", "reasoning", "stream"] {
                assert!(body.get(excluded).is_none());
            }
            assert_eq!(body["prompt_cache_options"], json!({"mode":"implicit"}));
            seen.lock().unwrap().push(body);
            arrived.notify_one();
            if mode == 2 { std::future::pending::<()>().await; }
            if mode == 6 && seen.lock().unwrap().len() == 1 { released.notified().await; }
            if mode == 8 {
                return (StatusCode::TOO_MANY_REQUESTS, [("retry-after", "0")], "busy").into_response();
            }
            if mode == 1 || mode == 6 { return (StatusCode::BAD_GATEWAY, [("retry-after", "0")], Json(json!({"error":"incomplete"}))).into_response(); }
            (StatusCode::OK, Json(json!({"object":"response.compaction","output":if mode==7 {json!([
                {"type":"function_call","call_id":"old","name":"TaskGet","arguments":"{}"},
                {"type":"function_call_output","call_id":"old","output":format!("old page!{}!","QUJD".repeat(256))},
                {"role":"user","content":[{"type":"input_text","text":format!("<task-notification>\n<kind>monitor</kind>\n<event>!{}!</event>\n</task-notification>","QUJD".repeat(256))}]},
                {"type":"compaction","encrypted_content":"opaque exact content"}
            ])} else {window()},"usage":{"input_tokens":150,"input_tokens_details":{"cached_tokens":112},"output_tokens":77}}))).into_response()
        }
    }));
    let captured = seen.clone();
    let read_fixture = dir.path().join("host-evidence.txt");
    let app = app.route("/tools/codex/v1/responses", post(move |headers: HeaderMap, Json(body): Json<Value>| {
        let over_budget = if mode == 8 && headers["x-dcl-request-kind"] == "turn" {
            let usage = ContextUsage::from_body("openai-responses", &body, Some(PRESSURE_WINDOW), PRESSURE_OUTPUT);
            let reserve = usage.sent_output_reserve.unwrap_or(u64::from(PRESSURE_OUTPUT));
            (usage.text_subtotal() + reserve > u64::from(PRESSURE_WINDOW)).then(|| format!(
                "mock rejected over-budget main request: history={}, full input={}, reserve={reserve}",
                usage.history.estimated_tokens, usage.text_subtotal()
            ))
        } else {
            None
        };
        let (host_summary, next_read) = if mode == 8 {
            let input = body["input"].to_string();
            let summary = headers.get("x-dcl-request-kind").is_some_and(|kind| kind == "compaction");
            let host_window = input.contains("HOST_SNAPSHOT_SUMMARY");
            let next_read = if host_window && !summary {
                let last_output = body["input"].as_array().unwrap().iter().rev()
                    .find(|row| row["type"] == "function_call_output");
                match last_output.map(|row| row["call_id"].as_str()) {
                    None => Some(0),
                    Some(Some("host-read-0")) => Some(1),
                    _ => None,
                }
            } else {
                None
            };
            (summary && (host_window || input.contains("HOST_ORIGINAL")), next_read)
        } else {
            (false, None)
        };
        let read_fixture = read_fixture.clone();
        let step = captured.lock().unwrap().len();
        let first = step == 0;
        if mode == 4 {
            // The Responses adapter sends the same filtered list: the tool the
            // fixture enabled, never an on-demand one nobody loaded.
            let names: Vec<&str> = body["tools"].as_array().unwrap().iter().filter_map(|tool| tool["name"].as_str()).collect();
            assert!(names.contains(&"ImageGenerate"), "{names:?}");
            assert!(names.contains(&"Read"), "{names:?}");
            assert!(!names.contains(&"ListDir"), "{names:?}");
        }
        if mode == 5 {
            // Step 0 is the compaction exchange; the first turn request follows
            // it with no restored load, and the search adds the tool from the
            // next request on.
            let names: Vec<String> = body["tools"].as_array().map(|tools| tools.iter().filter_map(|tool| tool["name"].as_str().map(str::to_owned)).collect()).unwrap_or_default();
            match step {
                1 => assert!(!names.contains(&"ListDir".to_owned()), "{names:?}"),
                _ => assert!(names.contains(&"ListDir".to_owned()), "{names:?}"),
            }
        }
        captured.lock().unwrap().push(body);
        async move {
            if let Some(message) = over_budget {
                eprintln!("{message}");
                return (StatusCode::BAD_REQUEST, message).into_response();
            }
            if matches!(mode, 1 | 6) {
                return (StatusCode::BAD_GATEWAY, [("retry-after", "0")], "unavailable").into_response();
            }
            let call = |id: &str, name: &str, arguments: Value| json!({"type":"function_call","id":format!("fc-{id}"),"call_id":id,"name":name,"arguments":arguments.to_string()});
            let read = next_read.is_some();
            let reply = if host_summary { "HOST_SNAPSHOT_SUMMARY" } else { "done" };
            let output = if let Some(index) = next_read {
                json!([call(&format!("host-read-{index}"), "Read", json!({"file_path":read_fixture, "offset":index + 1, "limit":1}))])
            } else if mode == 5 {
                match step {
                    1 => json!([call("search-1", "ToolLoad", json!({"names":["ListDir"]}))]),
                    2 => json!([call("list-1", "ListDir", json!({"path":"."}))]),
                    _ => json!([{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}]),
                }
            } else if mode == 4 && first {
                json!([{"type":"function_call","id":"fc-image","call_id":"call-image","name":"ImageGenerate","arguments":json!({"prompt":"Create an icon","file_path":"assets/图标.png"}).to_string()}])
            } else {
                json!([{"type":"message","role":"assistant","content":[{"type":"output_text","text":reply}]}])
            };
            let data = json!({"type":"response.completed","response":{"id":"response","status":"completed","output":output,"usage":{"input_tokens":80,"output_tokens":1}}});
            let tool_step = read || (mode == 4 && first) || (mode == 5 && step <= 2);
            let delta = if tool_step { String::new() } else { format!("event: response.output_text.delta\ndata: {}\n\n", json!({"type":"response.output_text.delta","delta":reply})) };
            ([("Content-Type", "text/event-stream")], format!("{delta}event: response.completed\ndata: {data}\n\n")).into_response()
        }
    }));
    let app = app.route(
        "/tools/codex/v1/images/generations",
        post(|headers: HeaderMap, Json(body): Json<Value>| async move {
            assert_eq!(headers["authorization"], "Bearer gateway-key");
            assert_eq!(body["model"], "gpt-image-2");
            Json(json!({"data":[{"b64_json":PNG}]}))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let provider: Provider = serde_json::from_value(json!({"id":"gateway","name":"gateway","builtin_id":"openai","api_format":"openai","base_url":format!("http://{}/tools/codex/v1",listener.local_addr().unwrap()),"api_key":"gateway-key","enabled":true,"models":[{"id":"gpt-6-astra","name":"model","api":"openai-responses","enabled":true,"supports_web_search":false,"max_tokens":8192,"context_window":if matches!(mode,3|7) {16000} else if mode == 8 {PRESSURE_WINDOW} else {256000}}]})).unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let permissions = runtime::default_permissions();
    permissions.set_mode(deepseek_code_core::permissions::PermissionMode::BypassPermissions);
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
    Fixture { dir, store, id, engine, seen, arrived, released, server }
}

// With the host context restored by 543b9515, this cut exceeds the single
// replay budget. Consume the native retries and both fragments
// before interpreting any remaining POST as a main request.
fn assert_pressure_fragments<'a>(
    calls: &'a [Value],
    attempts: usize,
    included: &[&str],
    excluded: &[&str],
) -> &'a [Value] {
    let (native, calls) = calls.split_at(attempts);
    for post in native {
        assert!(post.get("stream").is_none());
        assert_eq!(post, &native[0]);
    }

    let (fragments, remaining) = calls.split_at(2);
    let mut cut = String::new();
    for post in fragments {
        let text = summary_prompt(post);
        assert!(text.starts_with("Summarize this consecutive fragment"));
        cut.push_str(text.split_once("\n\nConversation fragment:\n").unwrap().1);
    }
    let native_input = native[0]["input"].to_string();
    for marker in included {
        assert!(
            native_input.contains(marker),
            "native cut omitted required input"
        );
        assert!(cut.contains(marker), "text fallback omitted required input");
    }
    for marker in excluded {
        assert!(
            !native_input.contains(marker),
            "native cut included {marker}"
        );
        assert!(!cut.contains(marker), "text fallback included {marker}");
    }
    remaining
}

fn summary_prompt(request: &Value) -> &str {
    assert_eq!(request["stream"], true);
    let input = request["input"].as_array().unwrap();
    assert_eq!(input.len(), 1);
    input[0]["content"][0]["text"].as_str().unwrap()
}

#[tokio::test]
async fn pressure_fallback_summarizes_only_the_cut_and_preserves_explicit_input() {
    use deepseek_code_core::native_engine::tool_loop::EngineEvent;
    let large_steer = format!("HOST_LIVE_STEER {}", "s".repeat(185_000));
    for (in_flight, steer) in [
        (false, ""),
        (true, ""),
        (true, "HOST_LIVE_STEER"),
        (true, large_steer.as_str()),
    ] {
        let repeats_compaction = steer == large_steer;
        let Fixture {
            dir,
            store,
            id,
            engine,
            seen,
            server,
            ..
        } = fixture(8).await;
        let prefix = format!("ARCHIVED_EVIDENCE {}", "a".repeat(185_000));
        if !in_flight {
            store.append(&id, "assistant", &prefix).unwrap();
            store
                .append_prompt(&id, "KEEP_CURRENT_INPUT", "typed")
                .unwrap();
        }
        let mut messages = store.model_history(&id, &ApiFormat::OpenAI).unwrap();
        if in_flight {
            std::fs::write(
                dir.path().join("host-evidence.txt"),
                "HOST_READ_ZERO\nHOST_READ_ONE\n",
            )
            .unwrap();
            messages = vec![json!({"role":"user","content":format!("HOST_ORIGINAL {prefix}")})];
        }
        let request = serde_json::from_value(json!({
            "conversation_id":id,"messages":messages,"model":"gpt-6-astra",
            "provider_id":"gateway","workspace_path":dir.path(),"max_tokens":1000,"max_turns":5,
        }))
        .unwrap();
        if !steer.is_empty() {
            use deepseek_code_core::services::input_queue::{enqueue, QueueMode, QueueOrigin};
            enqueue(
                &store.db,
                &id,
                "during-host-turn",
                json!(steer),
                QueueMode::Steer,
                QueueOrigin::User,
            )
            .unwrap();
        }
        let mut events = engine.send_message(request).await.unwrap();
        let mut completed = false;
        while let Some(event) = events.recv().await {
            assert!(!matches!(event, EngineEvent::Error(_)), "{event:?}");
            completed |= matches!(event, EngineEvent::MessageStop { ref full_text, .. } if full_text == "done");
        }
        assert!(completed);
        let boundaries = compaction::count(&store.db, &id).unwrap().proven();
        assert_eq!(boundaries, u64::from(!in_flight));
        let calls = seen.lock().unwrap();
        let attempts = deepseek_code_core::config::OrchestrationConfig::max_retries() as usize + 1;
        let after_fragments =
            assert_pressure_fragments(&calls, attempts, &[&prefix], &["KEEP_CURRENT_INPUT"]);
        let [merge, final_summary, after_summary @ ..] = after_fragments else {
            panic!("expected merge and final summary");
        };
        assert!(summary_prompt(merge).starts_with("Merge these summaries"));
        assert!(summary_prompt(final_summary).starts_with("Write one continuation summary"));
        let main = calls.last().unwrap()["input"].to_string();
        if in_flight {
            let [initial, first_read, rest @ ..] = after_summary else {
                panic!("expected initial main request and first Read continuation");
            };
            let remaining = if repeats_compaction {
                // The first exchange and steer now fit in the cut. Both fragment
                // replies match the first compaction, so merge and final are cached.
                assert_pressure_fragments(
                    rest,
                    attempts,
                    &["HOST_SNAPSHOT_SUMMARY", steer, "HOST_READ_ZERO"],
                    &["Tests remain pending", "HOST_READ_ONE"],
                )
            } else {
                rest
            };
            let [second_read] = remaining else {
                panic!(
                    "expected exactly one final Read continuation, got {}",
                    remaining.len()
                );
            };
            for (step, post) in [initial, first_read, second_read].into_iter().enumerate() {
                assert_eq!(post["stream"], true, "main {step}");
                let input = post["input"].as_array().unwrap();
                let text = post["input"].to_string();
                assert!(text.contains("HOST_SNAPSHOT_SUMMARY"), "main {step}");
                assert!(
                    !text.contains("Tests remain pending"),
                    "main {step} replayed the shard seed"
                );
                let after_second_summary = repeats_compaction && step == 2;
                if !after_second_summary {
                    assert_eq!(
                        text.matches("HOST_LIVE_STEER").count(),
                        usize::from(!steer.is_empty() && step > 0),
                        "main {step}"
                    );
                }
                for (index, marker) in ["HOST_READ_ZERO", "HOST_READ_ONE"]
                    .iter()
                    .take(step)
                    .enumerate()
                {
                    let id = format!("host-read-{index}");
                    let items: Vec<_> = input.iter().filter(|row| row["call_id"] == id).collect();
                    if after_second_summary && index == 0 && items.is_empty() {
                        // This complete exchange is represented by the new summary.
                        continue;
                    }
                    assert_eq!(items.len(), 2, "main {step}, call {id}");
                    assert_eq!(items[0]["type"], "function_call");
                    assert_eq!(items[1]["type"], "function_call_output");
                    assert!(
                        items[1]["output"].as_str().unwrap().contains(marker),
                        "main {step}, call {id}"
                    );
                }
            }
        } else {
            assert_eq!(after_summary.len(), 1);
            assert!(main.contains("done"));
        }
        assert_eq!(main.contains("KEEP_CURRENT_INPUT"), !in_flight);
        assert!(!main.contains("ARCHIVED_EVIDENCE"));
        eprintln!("host window={in_flight}, steer bytes={}, captured POSTs={}, durable boundaries={boundaries}",
            steer.len(), calls.len());
        server.abort();
    }
}

#[tokio::test]
async fn protected_exchange_must_fit_with_the_host_boundary_and_request_overhead() {
    use deepseek_code_core::{
        native_engine::tool_loop::EngineEvent,
        services::input_queue::{enqueue, QueueMode, QueueOrigin},
    };

    let Fixture {
        dir,
        store,
        id,
        engine,
        seen,
        server,
        ..
    } = fixture(8).await;
    std::fs::write(dir.path().join("host-evidence.txt"), "HOST_READ_ZERO\n").unwrap();
    let prefix = format!("HOST_ORIGINAL {}", "a".repeat(185_000));
    let request = serde_json::from_value(json!({
        "conversation_id":id, "messages":[{"role":"user","content":prefix}],
        "provider_id":"gateway", "model":"gpt-6-astra", "workspace_path":dir.path(),
        "max_tokens":PRESSURE_OUTPUT, "max_turns":5,
        "response_schema":{"type":"object","properties":{"result":{"type":"string"}},"required":["result"],"additionalProperties":false}
    })).unwrap();
    // Admitted after Read zero. These rows fit within the 95,000-token input
    // budget, but the instructions, tools and schema take the full wire over it.
    let steer = format!("PROTECTED_STEER {}", "s".repeat(247_500));
    enqueue(
        &store.db,
        &id,
        "large-steer",
        json!(steer),
        QueueMode::Steer,
        QueueOrigin::User,
    )
    .unwrap();
    let mut events = engine.send_message(request).await.unwrap();
    let mut error = None;
    let mut reads = Vec::new();
    while let Some(event) = events.recv().await {
        match event {
            EngineEvent::ToolUseDone {
                tool_name,
                output,
                is_error,
                ..
            } => {
                assert!(!is_error && output.contains("HOST_READ_ZERO"));
                reads.push(tool_name);
            }
            EngineEvent::Error(message) => error = Some(message),
            _ => {}
        }
    }
    let error = error.expect("expected a local capacity error after the Read");
    assert!(
        error.contains("only 95000 fit") && error.contains("/compact"),
        "{error}"
    );
    assert_eq!(reads, ["Read"]);
    eprintln!("protected exchange stopped locally: {error}");
    let calls = seen.lock().unwrap();
    let attempts = deepseek_code_core::config::OrchestrationConfig::max_retries() as usize + 1;
    let [merge, final_summary, main] =
        assert_pressure_fragments(&calls, attempts, &[&prefix], &["PROTECTED_STEER"])
    else {
        panic!("expected one main request after the host boundary was installed");
    };
    assert!(summary_prompt(merge).starts_with("Merge these summaries"));
    assert!(summary_prompt(final_summary).starts_with("Write one continuation summary"));
    let rows = main["input"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows[0]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("# Experience"));
    assert_eq!(rows[1]["content"][0]["text"], "HOST_SNAPSHOT_SUMMARY");
    assert!(main["text"]["format"]["schema"].is_object());
    assert_eq!(compaction::count(&store.db, &id).unwrap().proven(), 0);
    server.abort();
}

#[tokio::test]
async fn running_responses_turn_uses_native_compaction_before_local_truncation() {
    let Fixture {
        dir,
        store,
        id,
        engine,
        seen,
        server,
        ..
    } = fixture(3).await;
    deepseek_code_core::data_layout::write_config_file(
        dir.path().join("data/config/settings.json"),
        r#"{"requestRecording":{"enabled":true}}"#,
    )
    .unwrap();
    let original = "Detailed evidence ".repeat(2200);
    store.append(&id, "assistant", &original).unwrap();
    store.append(&id, "user", "Continue").unwrap();
    let request: ChatRequest = serde_json::from_value(json!({"conversation_id":id,"messages":store.model_history(&id,&ApiFormat::OpenAI).unwrap(),"model":"gpt-6-astra","provider_id":"gateway","workspace_path":dir.path(),"max_tokens":1000})).unwrap();
    let mut events = engine.send_message(request).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Some(event) = events.recv().await {
            assert!(!format!("{event:?}").starts_with("Error"), "{event:?}");
        }
    })
    .await
    .unwrap();
    let requests = seen.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[0]["input"].to_string().contains(&original));
    assert_eq!(requests[0]["prompt_cache_key"].as_str().unwrap().len(), 32);
    assert_eq!(requests[0]["prompt_cache_key"], requests[1]["prompt_cache_key"]);
    assert_eq!(
        &requests[1]["input"].as_array().unwrap()[..3],
        window().as_array().unwrap()
    );
    let records = dir
        .path()
        .join("data/diagnostics/request-records")
        .join(&id);
    let compact: Value =
        serde_json::from_slice(&std::fs::read(records.join("compaction-0001.json")).unwrap())
            .unwrap();
    assert_eq!(compact["trigger"], "auto");
    assert_eq!(compact["completed"], true);
    assert_eq!(compact["request_sequences"], json!([1]));
    assert_eq!(compact["after_native_content"]["items"], window());
    let meta_path = std::fs::read_dir(&records)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("0001-")
                && p.to_string_lossy().ends_with(".meta.json")
        })
        .unwrap();
    let meta: Value = serde_json::from_slice(&std::fs::read(&meta_path).unwrap()).unwrap();
    assert_eq!(meta["purpose"], "compaction");
    assert_eq!(meta["compaction_stage"], "native");
    let measured: (i64, i64) = store
        .db
        .shard_sql(&id, |c| {
            c.query_row(
                "SELECT input_tokens, cache_read_tokens FROM model_requests WHERE id=?1",
                [meta["model_request_id"].as_str().unwrap()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        })
        .unwrap()
        .unwrap();
    assert_eq!(measured, (150, 112));
    let body_path = meta_path
        .to_string_lossy()
        .replace(".meta.json", ".request.json");
    let body: Value = serde_json::from_slice(&std::fs::read(body_path).unwrap()).unwrap();
    assert_eq!(body, requests[0]);
    let response: Value = serde_json::from_slice(
        &std::fs::read(
            meta_path
                .to_string_lossy()
                .replace(".meta.json", ".response.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(response["output"], window());
    server.abort();
}

#[tokio::test]
async fn model_invokes_image_tool_and_receives_persisted_actual_image() {
    let Fixture { dir, store, id, engine, seen, server, .. } = fixture(4).await;
    let messages = store.model_history(&id, &ApiFormat::OpenAI).unwrap();
    let assistant_id = store.append(&id, "assistant", "").unwrap();
    let request: ChatRequest = serde_json::from_value(json!({
        "conversation_id":id, "messages":messages, "assistant_message_id":assistant_id,
        "model":"gpt-6-astra", "provider_id":"gateway", "workspace_path":dir.path()
    })).unwrap();
    let mut events = engine.send_message_with_history(request, false).await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Some(event) = events.recv().await {
            assert!(!format!("{event:?}").starts_with("Error"), "{event:?}");
            if let deepseek_code_core::native_engine::tool_loop::EngineEvent::ToolUseDone {
                tool_use_id,
                tool_name,
                tool_input,
                output,
                is_error,
            } = event
            {
                assert!(!is_error, "{output}");
                // Use the same host persistence entrypoint as CLI streaming results.
                store
                    .db
                    .shard_sql(&id, |conn| {
                        deepseek_code_core::db::message_repo::save_stream_tool_result(
                            conn,
                            &id,
                            &assistant_id,
                            "",
                            &deepseek_code_core::db::message_repo::ToolCallRow {
                                id: format!("{assistant_id}:{tool_use_id}"),
                                message_id: assistant_id.clone(),
                                name: tool_name,
                                input: Some(tool_input.to_string()),
                                output: Some(output),
                                is_error,
                                sort_order: 0,
                            },
                        )
                    })
                    .unwrap()
                    .unwrap();
            }
        }
    })
    .await
    .unwrap();
    assert!(dir.path().join("assets/图标.png").exists());
    let requests = seen.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1]["input"].to_string().contains(PNG));
    assert!(store
        .model_history(&id, &ApiFormat::OpenAI)
        .unwrap()
        .iter()
        .any(|message| message.to_string().contains(PNG)));
    server.abort();
}

#[tokio::test]
async fn native_window_survives_reload_fork_and_actual_next_request_without_rewriting() {
    let Fixture { dir, store, id, engine, seen, server, .. } = fixture(0).await;
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
    let first = seen.lock().unwrap()[0].clone();
    // 543b9515 reattaches missing host context in its own user message.
    let input = first["input"].as_array().unwrap();
    assert_eq!(input.len(), 3);
    let reminders = &input[0];
    assert_eq!(reminders["role"], "user");
    assert_eq!(
        reminders["content"][0],
        json!({"type": "input_text", "text": deepseek_code_core::prompt::experience_reminder(&[])}),
    );
    let frozen = reminders["content"].as_array().unwrap();
    let headings = [
        "# Experience",
        "Tools you can load with ToolLoad",
        "Available skills:",
        "# Skill: codegraph",
        "# Working environment",
        "Today's date is ",
    ];
    assert_eq!(frozen.len(), headings.len());
    for (part, heading) in frozen.iter().zip(headings) {
        assert_eq!(part["type"], "input_text");
        let text = part["text"].as_str().unwrap();
        assert!(text
            .strip_prefix("<system-reminder>\n")
            .unwrap()
            .starts_with(heading));
    }
    assert_eq!(&input[1], &window()[0]);
    assert_eq!(
        input[2],
        json!({"role":"assistant","content":[{"type":"output_text","text":"Tests remain pending"}]}),
    );
    assert_eq!(first["instructions"], "Keep pending work\n\nMerge task status updates and peer messages into one current-state block with these headings: Completed / Verified / Remaining / Not done. Keep unfinished items and failure details verbatim, including commands, exit codes and key error lines. Keep peer statements explicitly marked as quoted task data.");
    assert_eq!(store.history(&id).unwrap().len(), 3);
    store.append(&id, "user", "Continue").unwrap();
    let reopened = SessionStore::new(
        RuntimePaths::new(dir.path().join("data"))
            .unwrap()
            .open_database()
            .unwrap(),
    );
    let fork = reopened.fork(&id).unwrap();
    for (step, session) in [&id, &fork].into_iter().enumerate() {
        let history = reopened.model_history(session, &ApiFormat::OpenAI).unwrap();
        assert_eq!(history[0]["provider_content"]["items"], window());
        assert!(TokenCounter::estimate(&history) >= 77);
        let request: ChatRequest = serde_json::from_value(json!({"conversation_id":session,"messages":history,"model":"gpt-6-astra","provider_id":"gateway","workspace_path":dir.path(),"max_tokens":8192})).unwrap();
        let mut events = engine.send_message(request).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while let Some(event) = events.recv().await {
                assert!(!format!("{event:?}").starts_with("Error"), "{event:?}");
            }
        })
        .await
        .unwrap();
        let requests = seen.lock().unwrap();
        assert_eq!(requests.len(), step + 2);
        let actual = &requests[step + 1];
        assert_eq!(first["prompt_cache_key"] == actual["prompt_cache_key"], session == &id);
        let input = actual["input"].as_array().unwrap();
        assert_eq!(input.len(), 5);
        assert_eq!(&input[..3], window().as_array().unwrap());
        assert_eq!(input[3]["role"], "user");
        // Replay adds the current global and project indexes before the frozen date.
        let parts = input[3]["content"].as_array().unwrap();
        assert_eq!(parts.len(), 8);
        assert_eq!(&parts[..5], &frozen[..5]);
        for (part, scope) in parts[5..7].iter().zip(["global", "project"]) {
            assert_eq!(part["type"], "input_text");
            assert!(part["text"]
                .as_str()
                .unwrap()
                .starts_with(&format!("<system-reminder>\n<{scope}_memory>\n")));
        }
        assert_eq!(parts[7], frozen[5]);
        assert_eq!(
            input[4],
            json!({"role":"user","content":[{"type":"input_text","text":"Continue"}]})
        );
    }
    // A second native compact forwards the complete earlier window too.
    compaction::compact(&engine, &store.db, &id, "gateway", "gpt-6-astra", "")
        .await
        .unwrap();
    let requests = seen.lock().unwrap();
    assert_eq!(requests.len(), 4);
    let input = requests[3]["input"].as_array().unwrap();
    assert_eq!(input.len(), 6);
    assert_eq!(&input[..3], window().as_array().unwrap());
    assert_eq!(&input[3], reminders);
    assert_eq!(input[4], requests[1]["input"][4]);
    assert_eq!(
        input[5],
        json!({"role":"assistant","content":[{"type":"output_text","text":"done"}]})
    );
    server.abort();
}

fn opaque_state_count(store: &SessionStore,id:&str) -> i64 {
    store
        .db
        .shard_sql(&id, |conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM native_boundaries",
                [],
                |r| r.get::<_, i64>(0),
            )
        })
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn failure_and_cancellation_never_commit_opaque_state() {
    // Mode 1: the gateway answers with a real HTTP 502. The watchdog only keeps
    // a regression from hanging the suite; it is not part of the assertion.
    {
        let Fixture { dir: _dir, store, id, engine, seen, server, .. } = fixture(1).await;
        let data = _dir.path().join("data");
        assert!(data.is_dir());
        assert!(data.join("global.db").is_file());
        let before = store.history(&id).unwrap();
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            compaction::compact(&engine, &store.db, &id, "gateway", "gpt-6-astra", ""),
        )
        .await
        .expect("the mock 502 response should arrive within the watchdog")
        .expect_err("a gateway 502 must fail the compaction");
        let message = error.to_string();
        assert!(message.contains("HTTP 502"), "unexpected error: {message}");
        assert!(!message.contains("Elapsed"), "unexpected timeout: {message}");
        assert_eq!(store.history(&id).unwrap(), before);
        assert_eq!(
            seen.lock().unwrap().len(),
            2 * (deepseek_code_core::config::OrchestrationConfig::max_retries() as usize + 1)
        );
        assert_eq!(opaque_state_count(&store, &id), 0);
        assert!(data.join("global.db").is_file());
        server.abort();
    }

    // Mode 2: the mock never answers. Cancellation happens only after it has
    // validated and recorded the request, so the request count is exact.
    {
        let Fixture { dir: _dir, store, id, engine, seen, arrived, server, .. } = fixture(2).await;
        let data = _dir.path().join("data");
        assert!(data.is_dir());
        assert!(data.join("global.db").is_file());
        let before = store.history(&id).unwrap();
        let mut request = Box::pin(compaction::compact(
            &engine,
            &store.db,
            &id,
            "gateway",
            "gpt-6-astra",
            "",
        ));
        tokio::select! {
            _ = &mut request => panic!("compaction ended before its request reached the mock"),
            result = tokio::time::timeout(std::time::Duration::from_secs(10), arrived.notified()) => {
                assert!(result.is_ok(), "the mock never received the compaction request");
            }
        }
        // Drop the whole future, not just the Pin borrow: the pending response
        // must be abandoned without committing anything.
        drop(request);
        assert_eq!(store.history(&id).unwrap(), before);
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert_eq!(opaque_state_count(&store,&id), 0);
        assert!(data.join("global.db").is_file());
        server.abort();
    }
}

/// The old test allowed only 200 ms for the whole error response. Hold a
/// genuine 502 past that window to show the old check reports a timeout even
/// though the request had arrived and the released answer is a real error.
#[tokio::test]
async fn an_error_response_slower_than_200ms_would_misjudge_the_old_window() {
    let Fixture { dir: _dir, store, id, engine, seen, arrived, released, server, .. } =
        fixture(6).await;
    let data = _dir.path().join("data");
    assert!(data.is_dir());
    assert!(data.join("global.db").is_file());
    let before = store.history(&id).unwrap();
    let mut request = Box::pin(compaction::compact(
        &engine,
        &store.db,
        &id,
        "gateway",
        "gpt-6-astra",
        "",
    ));
    tokio::select! {
        _ = &mut request => panic!("compaction ended before its request reached the mock"),
        result = tokio::time::timeout(std::time::Duration::from_secs(10), arrived.notified()) => {
            assert!(result.is_ok(), "the mock never received the compaction request");
        }
    }
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), &mut request)
            .await
            .is_err(),
        "the old 200 ms window unexpectedly covered the delayed response"
    );
    released.notify_one();
    let error = tokio::time::timeout(std::time::Duration::from_secs(10), request)
        .await
        .expect("the released 502 response should arrive")
        .expect_err("a gateway 502 must fail the compaction");
    assert!(error.to_string().contains("HTTP 502"), "{error}");
    assert_eq!(store.history(&id).unwrap(), before);
    assert_eq!(
        seen.lock().unwrap().len(),
        2 * (deepseek_code_core::config::OrchestrationConfig::max_retries() as usize + 1)
    );
    assert_eq!(opaque_state_count(&store, &id), 0);
    assert!(data.join("global.db").is_file());
    server.abort();
}

/// A real compaction exchange replaces the history that carried a successful
/// ToolLoad, so the restored session cannot treat the summary as an
/// authorization; a fresh search loads the tool again for the next request.
#[tokio::test]
async fn a_compacted_search_record_cannot_authorize_and_a_fresh_search_restores_it() {
    let Fixture { dir, store, id, engine, seen, server, .. } = fixture(5).await;
    std::fs::write(dir.path().join("item.txt"), "listing").unwrap();
    let assistant = uuid::Uuid::new_v4().to_string();
    store.append(&id, "assistant", "").unwrap();
    store
        .db
        .shard_sql(&id, |conn| {
            deepseek_code_core::db::message_repo::save_stream_tool_result(
                conn,
                &id,
                &assistant,
                "",
                &deepseek_code_core::db::message_repo::ToolCallRow {
                    id: format!("{assistant}:search-1"),
                    message_id: assistant.clone(),
                    name: "ToolLoad".to_owned(),
                    input: Some(json!({"names":["ListDir"]}).to_string()),
                    output: Some(
                        json!({"success":true,"loaded_tools":["ListDir"],"tools":[]}).to_string(),
                    ),
                    is_error: false,
                    sort_order: 0,
                },
            )
        })
        .unwrap()
        .unwrap();
    store.append(&id, "user", "Continue").unwrap();
    let before = store.model_history(&id, &ApiFormat::OpenAI).unwrap();
    assert!(
        serde_json::to_string(&before).unwrap().contains("loaded_tools"),
        "{before:?}"
    );

    compaction::compact(&engine, &store.db, &id, "gateway", "gpt-6-astra", "")
        .await
        .unwrap();
    let after = store.model_history(&id, &ApiFormat::OpenAI).unwrap();
    assert!(
        !serde_json::to_string(&after).unwrap().contains("loaded_tools"),
        "the compaction window must replace the search record: {after:?}"
    );

    store.append(&id, "user", "Continue").unwrap();
    let request: ChatRequest = serde_json::from_value(json!({"conversation_id":id,"messages":store.model_history(&id,&ApiFormat::OpenAI).unwrap(),"model":"gpt-6-astra","provider_id":"gateway","workspace_path":dir.path(),"max_tokens":8192})).unwrap();
    let mut events = engine.send_message(request).await.unwrap();
    let mut errors = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Some(event) = events.recv().await {
            if let deepseek_code_core::native_engine::tool_loop::EngineEvent::ToolUseDone {
                tool_name,
                output,
                is_error,
                ..
            } = event
            {
                if is_error {
                    errors.push(format!("{tool_name}: {output}"));
                }
            }
        }
    })
    .await
    .unwrap();
    assert!(errors.is_empty(), "{errors:?}");
    let requests = seen.lock().unwrap();
    // The compaction exchange, then the first turn request without the tool,
    // the request after the fresh search, and the request carrying the listing.
    assert_eq!(requests.len(), 4, "{requests:#?}");
    let names = |body: &Value| -> Vec<String> {
        body["tools"]
            .as_array()
            .map(|tools| {
                tools
                    .iter()
                    .filter_map(|tool| tool["name"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    };
    assert!(!names(&requests[1]).contains(&"ListDir".to_owned()));
    assert!(names(&requests[2]).contains(&"ListDir".to_owned()));
    assert!(names(&requests[3]).contains(&"ListDir".to_owned()));
    assert!(
        requests[3].to_string().contains("item.txt"),
        "the restored search did not run the tool"
    );
    server.abort();
}

#[tokio::test]
async fn context_search_hint_requires_persisted_compaction_without_host_provenance() {
    let Fixture {
        dir,
        store,
        id,
        engine,
        seen,
        server,
        ..
    } = fixture(0).await;
    let send = |boundary: Option<&'static str>| {
        let history = store.model_history(&id, &ApiFormat::OpenAI).unwrap();
        let request: ChatRequest = serde_json::from_value(json!({
            "conversation_id":id,"messages":history,"model":"gpt-6-astra",
            "provider_id":"gateway","workspace_path":dir.path(),"context_boundary":boundary
        }))
        .unwrap();
        let engine = &engine;
        async move {
            let mut events = engine.send_message(request).await.unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                while let Some(event) = events.recv().await {
                    assert!(!format!("{event:?}").starts_with("Error"), "{event:?}");
                }
            })
            .await
            .unwrap();
        }
    };

    // A provenance field alone cannot make the fresh session compacted.
    send(Some("unverified-boundary")).await;
    let prepared = compaction::prepare(&store.db, &id).unwrap();
    compaction::commit(&store.db, prepared, "Host continuation summary").unwrap();
    store
        .append(&id, "user", "Continue from the summary")
        .unwrap();
    // Desktop supplies host messages without explicit boundary provenance.
    send(None).await;

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    let hint = "After compaction, load ContextSearch with ToolLoad";
    let fresh_system = seen[0]["instructions"].as_str().unwrap();
    assert!(!fresh_system.contains("Tools you can load with ToolLoad"), "{fresh_system}");
    let fresh = seen[0]["input"].to_string();
    assert!(fresh.contains("Tools you can load with ToolLoad"), "{fresh}");
    assert!(fresh.contains("- ContextSearch:"), "{fresh}");
    assert!(!fresh.contains(hint), "{fresh}");
    assert!(
        fresh.find("Tools you can load with ToolLoad").unwrap()
            < fresh.find("Today's date is").unwrap(),
        "the loadable tools reminder opens before the date: {fresh}"
    );
    let compacted_system = seen[1]["instructions"].as_str().unwrap();
    assert!(!compacted_system.contains(hint), "{compacted_system}");
    let compacted = seen[1]["input"].to_string();
    // The frozen first message still carries the same list after compaction.
    assert!(compacted.contains("- ContextSearch:"), "{compacted}");
    assert!(!compacted.contains(hint), "{compacted}");
    assert!(seen[1]["input"]
        .to_string()
        .contains("Host continuation summary"));
    server.abort();
}

#[tokio::test]
async fn base64_model_boundary_native_compaction_projects_stored_protocol_results() {
    let Fixture {
        dir: _dir,
        store,
        id,
        engine,
        seen,
        server,
        ..
    } = fixture(0).await;
    let assistant = store.append(&id, "assistant", "").unwrap();
    let payload = "QUJD".repeat(256);
    let page = format!("historical page\n!{payload}!");
    let ordinary = r#"{"stdout":"ordinary"}"#;
    store
        .db
        .shard_sql(&id, |conn| {
            for (index, (name, text)) in [("TaskGet", page.as_str()), ("Bash", ordinary)]
                .into_iter()
                .enumerate()
            {
                deepseek_code_core::db::message_repo::insert_tool_call(
                    conn,
                    &format!("call{index}"),
                    &assistant,
                    name,
                    Some("{}"),
                    Some(text),
                    false,
                    index as i64,
                )?;
            }
            Ok::<_, anyhow::Error>(())
        })
        .unwrap()
        .unwrap();
    let notice = format!("<task-notification>\n<kind>monitor</kind>\n<event>!{payload}!</event>\n</task-notification>");
    store.append(&id, "user", &notice).unwrap();
    compaction::compact(&engine, &store.db, &id, "gateway", "gpt-6-astra", "")
        .await
        .unwrap();
    let request = seen.lock().unwrap()[0].clone();
    let items = request["input"].as_array().unwrap();
    let page_view = items
        .iter()
        .filter(|i| i["type"] == "function_call_output")
        .find_map(|i| {
            i["output"]
                .as_str()
                .filter(|text| text.starts_with("historical page"))
        })
        .unwrap();
    let notice_view = items
        .iter()
        .filter(|i| i["role"] == "user")
        .find_map(|i| {
            i["content"][0]["text"]
                .as_str()
                .filter(|text| text.starts_with("<task-notification>"))
        })
        .unwrap();
    for (view, raw, number) in [
        (page_view, page.as_str(), 2),
        (notice_view, notice.as_str(), 3),
    ] {
        assert!(!view.contains(&payload));
        let (file, line) = view
            .split_once("; full text: ")
            .unwrap()
            .1
            .split_once(", line ")
            .unwrap();
        assert_eq!(line.split_once(']').unwrap().0, number.to_string());
        assert_eq!(std::fs::read_to_string(file).unwrap(), raw);
    }
    assert!(items.iter().any(|i| i["output"] == ordinary));
    server.abort();
    let _ = server.await;
}

#[path = "native_compaction/model_output.rs"]
mod model_output;

#[tokio::test]
async fn compaction_consolidates_only_archived_rows_and_search_recovers_the_original(
) -> anyhow::Result<()> {
    use deepseek_code_core::db::message_repo::{insert_tool_call, UNFINISHED_TOOL_RESULT};
    let f = fixture(8).await;
    let store = &f.store;
    let id = &f.id;
    let notice = |task: &str, text: &str| {
        format!(
        "<task-notification>\n<task-id>{task}</task-id>\n<summary>{text}</summary>\n</task-notification>"
    )
    };
    for (task, text, origin) in [
        ("a", "earliest notice", "task_notification"),
        ("b", "other task", "task_notification"),
        ("a", "middle notice", "task_notification"),
        ("a", "typed quotation", "typed"),
        ("a", "latest notice", "task_notification"),
    ] {
        let msg = notice(task, text);
        store.append_user_content(id, &msg, &json!([{"type":"text","text":msg}]), origin)?;
    }
    let diagnostic = "build --release\nexit code 9\nerror: missing target";
    let failed_notice = format!("<task-notification>\n<task-id>retry</task-id>\n<status>failed</status>\n<result>{diagnostic}</result>\n</task-notification>");
    store.append_prompt(id, &failed_notice, "task_notification")?;
    store.append_prompt(id, &notice("retry", "retry complete"), "task_notification")?;
    let old = format!("ARCHIVED_NEEDLE\n{}\nfin🙂", "word ".repeat(260));
    let failed = format!(
        "compile --strict\nexit code 17\nerror: expected module\n{}",
        "trace ".repeat(220)
    );
    let recent = format!("recent read\n{}", "data ".repeat(220));
    let tool = |message: &str, call: &str, text: Option<&str>, error: bool, order: i64| {
        store.db.shard_sql(id, |db| {
            insert_tool_call(
                db,
                call,
                message,
                "Read",
                Some(r#"{"file_path":"src/main.rs"}"#),
                text,
                error,
                order,
            )
        })?
    };
    let first = store.append(id, "assistant", "Remaining: verify the fixture.")?;
    tool(&first, "old", Some(&old), false, 0)?;
    tool(&first, "failed", Some(&failed), true, 1)?;
    tool(&first, "interrupted", None, false, 2)?;
    let hook_failure = format!(
        "{}\nPostToolUseFailure context: inspect dependency",
        json!({"exit_code":42,"stderr":format!("missing dependency {}", "trace ".repeat(200))})
    );
    tool(&first, "hook-failure", Some(&hook_failure), false, 3)?;
    for i in 0..9 {
        store.append_prompt(id, &format!("Turn {i}"), "typed")?;
        let replied = store.append(id, "assistant", "Read the previous result.")?;
        if i == 0 {
            tool(&replied, "recent", Some(&recent), false, 0)?;
        }
    }
    store.append_prompt(id, "Continue the unfinished work.", "typed")?;
    let mut turn = store.db.start_turn_timer(id)?;
    let current = turn.ensure_segment()?;
    tool(&current, "current", Some(&old), false, 0)?;
    for text in ["active first", "active latest"] {
        store.append_prompt(id, &notice("live", text), "task_notification")?;
    }
    let before = || -> anyhow::Result<_> {
        Ok(serde_json::to_vec(
            &store.model_history(id, &ApiFormat::OpenAI)?,
        )?)
    };
    let normal = before()?;
    let prepared = compaction::prepare(&store.db, id)?;
    let generated = f
        .engine
        .generate_compaction(
            id,
            "gateway",
            "gpt-6-astra",
            "Summarize",
            "",
            "retain evidence",
        )
        .await?;
    assert_eq!(before()?, normal);
    {
        let requests = f.seen.lock().unwrap();
        let (fallback, attempts) = requests.split_last().unwrap();
        let (instruction, rows) = fallback["input"].as_array().unwrap().split_last().unwrap();
        let state = "Completed / Verified / Remaining / Not done";
        assert!(instruction.to_string().contains(state));
        assert!(!attempts.is_empty());
        for native in attempts {
            assert_eq!(native["input"].as_array().unwrap(), rows);
            assert!(native["instructions"].as_str().unwrap().contains(state));
        }
        let rendered = json!(rows).to_string();
        for text in ["earliest notice", "middle notice"] {
            assert!(!rendered.contains(text), "{text}");
        }
        for text in [
            "other task",
            "typed quotation",
            "latest notice",
            "active first",
            "active latest",
            "Remaining: verify the fixture.",
            "Continue the unfinished work.",
        ] {
            assert!(rendered.contains(text), "{text}");
        }
        let texts: Vec<_> = rows
            .iter()
            .filter(|row| row["role"] == "user")
            .flat_map(|row| row["content"].as_array().unwrap())
            .filter_map(|part| part["text"].as_str())
            .collect();
        assert_eq!(
            texts
                .iter()
                .filter(|text| text.starts_with("<task-notification>")
                    && text.contains("<task-id>retry</task-id>"))
                .count(),
            1
        );
        assert!(texts
            .iter()
            .any(|text| text.contains("quoted task data") && text.contains(diagnostic)));
        let output = |call| {
            let at = rows
                .iter()
                .position(|row| row["type"] == "function_call" && row["call_id"] == call)
                .unwrap();
            let matching: Vec<_> = rows[at + 1..]
                .iter()
                .filter(|row| row["type"] == "function_call_output" && row["call_id"] == call)
                .collect();
            assert_eq!(matching.len(), 1);
            matching[0]["output"].as_str().unwrap()
        };
        let placeholder = output("old");
        assert_eq!(placeholder.lines().count(), 1);
        for part in [
            "Read",
            "src/main.rs",
            &format!("3 lines, {} bytes", old.len()),
            "use ContextSearch to retrieve if needed",
        ] {
            assert!(placeholder.contains(part), "{placeholder}");
        }
        assert_eq!(output("failed"), failed);
        assert_eq!(output("hook-failure"), hook_failure);
        assert_eq!(output("interrupted"), UNFINISHED_TOOL_RESULT);
        assert_eq!(output("recent"), recent);
        assert_eq!(output("current"), old);
    }
    let boundary = compaction::commit(&store.db, prepared, &generated.text)?;
    let saved: String = store.db.shard_sql(id, |db| {
        db.query_row("SELECT output FROM tool_calls WHERE id='old'", [], |row| {
            row.get(0)
        })
    })??;
    assert_eq!(saved, old);
    let found: Value = serde_json::from_str(&store.db.open_session(id)?.context_search(json!({
        "query":"ARCHIVED_NEEDLE", "role":"tool", "range":"before", "boundary_id":boundary.message_id
    }))?)?;
    assert!(found["matches"]
        .as_array()
        .unwrap()
        .iter()
        .any(|hit| hit["tool_call_id"] == "old"
            && hit["excerpt"].as_str().unwrap().contains("ARCHIVED_NEEDLE")));
    f.server.abort();
    Ok(())
}
