use super::*;
use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, Mutex,
};

#[derive(Default)]
struct Fixture {
    requests: Mutex<Vec<Value>>,
    reply: Mutex<(u16, String)>,
    delay: AtomicU64,
}

async fn endpoint(
    State(state): State<Arc<Fixture>>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let anthropic = body.get("system").is_some();
    state.requests.lock().unwrap().push(body);
    tokio::time::sleep(Duration::from_millis(state.delay.load(Ordering::Relaxed))).await;
    let (status, text) = state.reply.lock().unwrap().clone();
    let body = if anthropic {
        json!({"id":"hook","type":"message","role":"assistant","model":"fixture","content":[{"type":"text","text":text}],"stop_reason":"end_turn"})
    } else {
        json!({"id":"hook","object":"chat.completion","created":0,"model":"fixture","choices":[{"index":0,"message":{"role":"assistant","content":text},"finish_reason":"stop"}]})
    };
    (StatusCode::from_u16(status).unwrap(), Json(body))
}

fn provider(url: &str, format: &str) -> ResolvedProvider {
    let provider: crate::native_engine::provider_manager::Provider = serde_json::from_value(json!({"id":"fixture","name":"fixture","base_url":url,"api_key":"fixture-only","api_format":format,"enabled":true,"models":[{"id":"fixture","name":"fixture","enabled":true,"supports_vision":false,"supports_web_search":false},{"id":"judge","name":"judge","enabled":true,"supports_vision":false,"supports_web_search":false}]})).unwrap();
    ResolvedProvider {
        model: provider.models[0].clone(),
        provider,
    }
}

async fn fixture() -> (String, Arc<Fixture>, tokio::task::JoinHandle<()>) {
    let state = Arc::new(Fixture::default());
    *state.reply.lock().unwrap() = (200, "{\"ok\":true}".into());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/v1/chat/completions", post(endpoint))
        .route("/v1/messages", post(endpoint))
        .with_state(state.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (url, state, server)
}

fn runner(root: &Path, provider: ResolvedProvider, extra: Value) -> HookRunner {
    let mut hook = json!({"type":"prompt","prompt":"Evaluate $ARGUMENTS","timeout":1});
    hook.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    crate::data_layout::write_config_file(
        crate::data_layout::DataLayout::from_root(&root).config_file("settings.json"),
        json!({"hooks":{"PreToolUse":[{"matcher":"Read","hooks":[hook]}]}}).to_string(),
    )
    .unwrap();
    HookRunner::load(root, root)
        .unwrap()
        .with_prompt_provider(provider)
        .unwrap()
}

async fn run(runner: &HookRunner, session: &str) -> HookOutcome {
    runner
        .run(
            HookEvent::PreToolUse,
            session,
            json!({"tool_name":"Read","tool_input":{"file_path":"a.txt"}}),
        )
        .await
        .unwrap()
}

#[derive(Default)]
struct BuiltinFixture {
    state: Fixture,
    transient_failures: AtomicU64,
}

async fn builtin_endpoint(
    State(fixture): State<Arc<BuiltinFixture>>,
    Json(body): Json<Value>,
) -> (StatusCode, [(&'static str, &'static str); 2], String) {
    let anthropic = body.get("system").is_some();
    fixture.state.requests.lock().unwrap().push(body);
    if fixture
        .transient_failures
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
        .is_ok()
    {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            [("content-type", "application/json"), ("retry-after", "0")],
            "{}".into(),
        );
    }
    tokio::time::sleep(Duration::from_millis(
        fixture.state.delay.load(Ordering::Relaxed),
    ))
    .await;
    let (status, text) = fixture.state.reply.lock().unwrap().clone();
    let events = if anthropic {
        vec![
            json!({"type":"message_start","message":{"id":"hook","model":"fixture","usage":{"input_tokens":1}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}),
            json!({"type":"message_stop"}),
        ]
    } else {
        vec![json!({"choices":[{"index":0,"delta":{"content":text},"finish_reason":"stop"}]})]
    };
    let stream = events
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();
    (
        StatusCode::from_u16(status).unwrap(),
        [("content-type", "text/event-stream"), ("retry-after", "5")],
        stream,
    )
}

async fn builtin_fixture(
    id: &str,
) -> (
    ResolvedProvider,
    Arc<BuiltinFixture>,
    tokio::task::JoinHandle<()>,
) {
    let state = Arc::new(BuiltinFixture::default());
    *state.state.reply.lock().unwrap() = (200, "{\"ok\":true}".into());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1", listener.local_addr().unwrap());
    let format = if id == "anthropic" {
        "anthropic"
    } else {
        "openai"
    };
    let mut provider = provider(&url, format);
    provider.provider.builtin_id = Some(id.into());
    let api = if id == "anthropic" {
        "anthropic-messages"
    } else {
        "openai-completions"
    };
    for model in &mut provider.provider.models {
        model.api = Some(api.into());
    }
    provider.model = provider.provider.models[0].clone();
    let app = Router::new()
        .route("/v1/chat/completions", post(builtin_endpoint))
        .route("/v1/messages", post(builtin_endpoint))
        .with_state(state.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (provider, state, server)
}

#[tokio::test]
async fn builtins_evaluate_allow_and_deny_through_native_streaming_transport() {
    for id in ["deepseek", "anthropic"] {
        let (provider, fixture, server) = builtin_fixture(id).await;
        let root = tempfile::tempdir().unwrap();
        let runner = runner(root.path(), provider, json!({"model":"judge"}));
        let out = run(&runner, "builtin-allow").await;
        assert!(
            !out.blocked && !out.allow_permission && out.warnings.is_empty(),
            "{id}: {out:?}"
        );
        *fixture.state.reply.lock().unwrap() =
            (200, "{\"ok\":false,\"reason\":\"builtin denied\"}".into());
        let out = run(&runner, "builtin-deny").await;
        assert!(out.blocked && out.prevent_continuation && !out.allow_permission);
        assert!(out.reason.contains("builtin denied"));
        let requests = fixture.state.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for request in requests.iter() {
            assert_eq!(
                request["stream"], true,
                "Must use provider_runtime for {id}"
            );
            assert_eq!(request["model"], "judge");
            assert_eq!(request["max_tokens"], MAX_TOKENS);
            assert!(request.get("tools").is_none() && request.get("tool_choice").is_none());
        }
        server.abort();
    }
}

#[tokio::test]
async fn builtin_retries_and_slow_responses_share_hook_deadline() {
    for id in ["deepseek", "anthropic"] {
        let (provider, fixture, server) = builtin_fixture(id).await;
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let runner = runner(
            root.path(),
            provider,
            json!({"statusMessage":"Builtin evaluation","once":true}),
        );
        let progress = crate::services::hook_progress::subscribe(&session);
        // A transient failure can retry within one evaluation.
        fixture.transient_failures.store(1, Ordering::Relaxed);
        let out = run(&runner, "retry-success").await;
        assert!(out.warnings.is_empty() && !out.allow_permission, "{out:?}");
        assert_eq!(fixture.state.requests.lock().unwrap().len(), 2);
        // Retry-After exceeds the hook deadline; the retry sleep must be cancelled.
        *fixture.state.reply.lock().unwrap() = (429, "{}".into());
        let before = fixture.state.requests.lock().unwrap().len();
        let out = tokio::time::timeout(Duration::from_secs(3), run(&runner, &session))
            .await
            .expect("Hook must bound transport retries");
        assert!(!out.blocked && !out.allow_permission && out.warnings[0].contains("timed out"));
        assert_eq!(fixture.state.requests.lock().unwrap().len(), before + 1);
        assert!(progress.snapshot().is_empty());
        *fixture.state.reply.lock().unwrap() = (200, "{\"ok\":true}".into());
        fixture.state.delay.store(4000, Ordering::Relaxed);
        let out = tokio::time::timeout(Duration::from_secs(3), run(&runner, &session))
            .await
            .expect("Hook must bound response wait");
        assert!(!out.allow_permission && out.warnings[0].contains("timed out"));
        assert!(progress.snapshot().is_empty());
        fixture.state.delay.store(0, Ordering::Relaxed);
        assert!(
            run(&runner, &session).await.warnings.is_empty(),
            "Timeout must not consume once"
        );
        server.abort();
    }
}

#[tokio::test]
async fn both_protocols_exact_requests_decisions_and_once_reload() {
    for format in ["openai", "anthropic"] {
        let (url, state, server) = fixture().await;
        let root = tempfile::tempdir().unwrap();
        let provider = provider(&url, format);
        let runner = runner(
            root.path(),
            provider.clone(),
            json!({"once":true,"if":"Read","model":"judge"}),
        );
        runner
            .run(HookEvent::PreToolUse, "one", json!({"tool_name":"Write"}))
            .await
            .unwrap();
        assert!(state.requests.lock().unwrap().is_empty());
        let out = run(&runner, "one").await;
        assert!(!out.blocked && !out.allow_permission && out.warnings.is_empty());
        let reloaded = HookRunner::load(root.path(), root.path())
            .unwrap()
            .with_prompt_provider(provider)
            .unwrap();
        run(&reloaded, "one").await;
        assert_eq!(state.requests.lock().unwrap().len(), 1);
        let request = state.requests.lock().unwrap()[0].clone();
        assert_eq!(request["model"], "judge");
        assert_eq!(request["max_tokens"], 1024);
        for field in ["tools", "tool_choice", "thinking", "stream"] {
            assert!(request.get(field).is_none(), "{request}");
        }
        let messages = request["messages"].as_array().unwrap();
        assert_eq!(messages.len(), if format == "openai" { 2 } else { 1 });
        let prompt = messages.last().unwrap()["content"].as_str().unwrap();
        let payload: Value =
            serde_json::from_str(prompt.strip_prefix("Evaluate ").unwrap()).unwrap();
        assert_eq!(payload["session_id"], "one");
        assert_eq!(payload["tool_name"], "Read");
        assert_eq!(payload["tool_input"], json!({"file_path":"a.txt"}));
        assert_eq!(payload["hook_event_name"], "PreToolUse");
        if format == "openai" {
            assert_eq!(messages[0]["content"], SYSTEM);
            assert_eq!(
                request["response_format"]["json_schema"]["schema"]["required"],
                json!(["ok"])
            );
        } else {
            assert_eq!(request["system"], SYSTEM);
            assert_eq!(
                request["output_config"]["format"]["schema"]["required"],
                json!(["ok"])
            );
        }
        *state.reply.lock().unwrap() =
            (200, "{\"ok\":false,\"reason\":\"blocked fixture\"}".into());
        let out = run(&runner, "two").await;
        assert!(out.blocked && out.prevent_continuation && !out.allow_permission);
        assert!(out.reason.contains("blocked fixture"));
        run(&reloaded, "two").await;
        assert_eq!(
            state.requests.lock().unwrap().len(),
            3,
            "blocking result must not consume once"
        );
        server.abort();
    }
}

#[tokio::test]
async fn invalid_output_transport_timeout_and_cancel_never_grant_or_consume_once() {
    for format in ["openai", "anthropic"] {
        let (url, state, server) = fixture().await;
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let runner = Arc::new(runner(
            root.path(),
            provider(&url, format),
            json!({"once":true,"statusMessage":"Judging"}),
        ));
        let mut progress = crate::services::hook_progress::subscribe(&session);
        for (status, text) in [
            (200, "bad JSON"),
            (200, "{}"),
            (200, "{\"ok\":\"true\"}"),
            (200, "{\"ok\":true,\"reason\":null}"),
            (400, "{}"),
            (500, "{}"),
        ] {
            *state.reply.lock().unwrap() = (status, text.into());
            let before = state.requests.lock().unwrap().len();
            let out = run(&runner, &session).await;
            assert!(!out.blocked && !out.allow_permission && !out.warnings.is_empty());
            assert_eq!(state.requests.lock().unwrap().len(), before + 1);
            assert!(progress.snapshot().is_empty());
        }
        state.delay.store(3000, Ordering::Relaxed);
        assert!(run(&runner, &session).await.warnings[0].contains("timed out"));
        assert!(progress.snapshot().is_empty());
        let worker = runner.clone();
        let worker_session = session.clone();
        let task = tokio::spawn(async move { run(&worker, &worker_session).await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while progress.snapshot().is_empty() {
                progress.changed().await;
            }
        })
        .await
        .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(progress.snapshot().is_empty());
        state.delay.store(0, Ordering::Relaxed);
        *state.reply.lock().unwrap() = (200, "{\"ok\":true,\"decision\":\"approve\"}".into());
        let out = run(&runner, &session).await;
        assert!(!out.allow_permission && out.warnings.is_empty());
        server.abort();
    }
}

#[test]
fn configuration_events_options_models_and_argument_contract() {
    for event in [
        "SessionStart",
        "SessionEnd",
        "SubagentStart",
        "PreCompact",
        "PostCompact",
        "WorktreeCreate",
        "WorktreeRemove",
        "PermissionDenied",
        "Notification",
        "Unknown",
    ] {
        let root = tempfile::tempdir().unwrap();
        crate::data_layout::write_config_file(
            crate::data_layout::DataLayout::from_root(&root.path()).config_file("settings.json"),
            json!({"hooks":{event:[{"hooks":[{"type":"prompt","prompt":"check"}]}]}}).to_string(),
        )
        .unwrap();
        assert!(
            HookRunner::load(root.path(), root.path()).is_err(),
            "{event}"
        );
    }
    for extra in [
        json!({"async":false}),
        json!({"asyncRewake":true}),
        json!({"shell":"sh"}),
        json!({"timeout":0}),
        json!({"timeout":0.5}),
        json!({"prompt":""}),
    ] {
        let mut value = json!({"type":"prompt","prompt":"check"});
        value
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let root = tempfile::tempdir().unwrap();
        crate::data_layout::write_config_file(
            crate::data_layout::DataLayout::from_root(&root.path()).config_file("settings.json"),
            json!({"hooks":{"PreToolUse":[{"hooks":[value]}]}}).to_string(),
        )
        .unwrap();
        assert!(
            HookRunner::load(root.path(), root.path()).is_err(),
            "{extra}"
        );
    }
    let config = PromptHook {
        kind: "prompt".into(),
        prompt: "check".into(),
        model: Some("missing".into()),
    };
    assert!(select_provider(&config, &provider("http://127.0.0.1:1", "openai")).is_err());
    assert_eq!(arguments("check", "{}"), "check\n\nARGUMENTS: {}");
    assert_eq!(arguments("$ARGUMENTS|$ARGUMENTS", "{}"), "{}|{}");
    assert_eq!(
        arguments("$0|$ARGUMENTS[1]|$9|$1word", "'two words' next"),
        "two words|next||$1word"
    );
    assert_eq!(
        arguments("$0", "{\"tool_name\":\"Read\"}"),
        "{tool_name:Read}"
    );
}

#[tokio::test]
async fn prompt_command_and_sdk_keep_deny_and_ask_priority() {
    use crate::services::hook_callbacks::{self, Host};
    struct Deny;
    impl Host for Deny {
        fn invoke<'a>(
            &'a self,
            _: &'a str,
            _: Value,
            _: Option<String>,
        ) -> futures::future::BoxFuture<'a, Result<Value>> {
            Box::pin(async {
                Ok(
                    json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"SDK denied"}}),
                )
            })
        }
    }
    let (url, state, server) = fixture().await;
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("hook.cjs"), "process.stdout.write(JSON.stringify({hookSpecificOutput:{hookEventName:'PreToolUse',permissionDecision:process.argv[2]}}));").unwrap();
    crate::data_layout::write_config_file(crate::data_layout::DataLayout::from_root(&root.path()).config_file("settings.json"), json!({"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"node hook.cjs allow"},{"type":"command","command":"node hook.cjs ask"},{"type":"prompt","prompt":"check"}]}]}}).to_string()).unwrap();
    let provider = provider(&url, "openai");
    let runner = HookRunner::load(root.path(), root.path())
        .unwrap()
        .with_prompt_provider(provider.clone())
        .unwrap();
    let out = run(&runner, "ask").await;
    assert!(out.ask_permission && out.allow_permission && !out.blocked);
    *state.reply.lock().unwrap() = (200, "{\"ok\":false,\"reason\":\"prompt denied\"}".into());
    let out = run(&runner, "deny").await;
    assert!(out.blocked && out.ask_permission && out.allow_permission);
    *state.reply.lock().unwrap() = (200, "{\"ok\":true}".into());
    let session = uuid::Uuid::new_v4().to_string();
    let host: Arc<dyn Host> = Arc::new(Deny);
    let _guard = hook_callbacks::install(
        &session,
        hook_callbacks::validate(&json!({"PreToolUse":[{"hookCallbackIds":["deny"]}]})).unwrap(),
        host.clone(),
    );
    let runner = runner.with_session_callbacks(&session);
    let out = run(&runner, &session).await;
    assert!(out.blocked && out.ask_permission && out.allow_permission);
    assert_eq!(out.reason, "SDK denied");
    server.abort();
}

#[tokio::test]
async fn permission_request_waits_for_prompt_guard_before_callback_approval() {
    use crate::services::hook_callbacks::{self, Host};
    struct Allow;
    impl Host for Allow {
        fn invoke<'a>(
            &'a self,
            _: &'a str,
            _: Value,
            _: Option<String>,
        ) -> futures::future::BoxFuture<'a, Result<Value>> {
            Box::pin(async {
                Ok(
                    json!({"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}),
                )
            })
        }
    }
    let (url, state, server) = fixture().await;
    *state.reply.lock().unwrap() = (200, "{\"ok\":false,\"reason\":\"guard denied\"}".into());
    state.delay.store(100, Ordering::Relaxed);
    let root = tempfile::tempdir().unwrap();
    crate::data_layout::write_config_file(
        crate::data_layout::DataLayout::from_root(&root.path()).config_file("settings.json"),
        json!({"hooks":{"PermissionRequest":[{"hooks":[{"type":"prompt","prompt":"check"}]}]}})
            .to_string(),
    )
    .unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    let host: Arc<dyn Host> = Arc::new(Allow);
    let _guard = hook_callbacks::install(
        &session,
        hook_callbacks::validate(&json!({"PermissionRequest":[{"hookCallbackIds":["allow"]}]}))
            .unwrap(),
        host.clone(),
    );
    let runner = HookRunner::load(root.path(), root.path())
        .unwrap()
        .with_prompt_provider(provider(&url, "openai"))
        .unwrap()
        .with_session_callbacks(&session);
    let out = runner
        .run(
            HookEvent::PermissionRequest,
            &session,
            json!({"tool_name":"Write"}),
        )
        .await
        .unwrap();
    assert!(matches!(
        out.permission_response,
        Some(HookPermissionResponse::Deny { .. })
    ));
    server.abort();
}
