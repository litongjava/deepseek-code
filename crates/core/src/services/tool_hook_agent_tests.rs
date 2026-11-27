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
    reads: AtomicU64,
    tool: Mutex<String>,
}

async fn endpoint(
    State(state): State<Arc<Fixture>>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let anthropic = body.get("system").is_some();
    state.requests.lock().unwrap().push(body);
    tokio::time::sleep(Duration::from_millis(state.delay.load(Ordering::Relaxed))).await;
    let (status, text) = state.reply.lock().unwrap().clone();
    let read = state
        .reads
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
        .is_ok();
    let tool: Value = if read {
        json!({"file_path":"a.txt"})
    } else {
        serde_json::from_str(&text).unwrap_or(json!({"ok":"invalid"}))
    };
    let name = if read {
        state.tool.lock().unwrap().clone()
    } else {
        "StructuredOutput".into()
    };
    let body = if anthropic {
        json!({"id":"hook","type":"message","role":"assistant","model":"fixture","content":[{"type":"tool_use","id":"result","name":name,"input":tool}],"stop_reason":"tool_use"})
    } else {
        json!({"id":"hook","object":"chat.completion","created":0,"model":"fixture","choices":[{"index":0,"message":{"role":"assistant","content":"","tool_calls":[{"id":"result","type":"function","function":{"name":name,"arguments":tool.to_string()}}]},"finish_reason":"tool_calls"}]})
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
    let mut hook = json!({"type":"agent","prompt":"Evaluate $ARGUMENTS","timeout":1});
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

#[tokio::test]
async fn agent_command_and_sdk_keep_deny_and_ask_priority() {
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
    crate::data_layout::write_config_file(crate::data_layout::DataLayout::from_root(&root.path()).config_file("settings.json"), json!({"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"node hook.cjs allow"},{"type":"command","command":"node hook.cjs ask"},{"type":"agent","prompt":"check"}]}]}}).to_string()).unwrap();
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
async fn permission_request_waits_for_agent_guard_before_callback_approval() {
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
        json!({"hooks":{"PermissionRequest":[{"hooks":[{"type":"agent","prompt":"check"}]}]}})
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

#[tokio::test]
async fn multi_turn_read_deny_tools_and_once_both_protocols() {
    for format in ["openai", "anthropic"] {
        let (url, state, server) = fixture().await;
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.txt"), "workspace evidence").unwrap();
        let provider = provider(&url, format);
        let runner = runner(
            root.path(),
            provider.clone(),
            json!({"once":true,"if":"Read","statusMessage":"Verifying"}),
        );
        runner
            .run(HookEvent::PreToolUse, "skip", json!({"tool_name":"Write"}))
            .await
            .unwrap();
        assert!(state.requests.lock().unwrap().is_empty());
        for tool in [
            "Read",
            "Write",
            "Bash",
            "Agent",
            "AskUserQuestion",
            "mcp__x__read",
        ] {
            let session = uuid::Uuid::new_v4().to_string();
            state.reads.store(1, Ordering::Relaxed);
            *state.tool.lock().unwrap() = tool.into();
            let before = state.requests.lock().unwrap().len();
            let out = run(&runner, &session).await;
            assert!(
                !out.blocked && !out.allow_permission && out.warnings.is_empty(),
                "{out:?}"
            );
            let requests = state.requests.lock().unwrap();
            assert_eq!(requests.len(), before + 2);
            let second = requests.last().unwrap().to_string();
            assert!(
                second.contains(if tool == "Read" {
                    "workspace evidence"
                } else {
                    "permission denied"
                }),
                "{second}"
            );
            assert_eq!(requests[before]["max_tokens"], MAX_TOKENS);
            assert_eq!(requests[before]["tools"].as_array().unwrap().len(), 2);
            drop(requests);
            let reload = HookRunner::load(root.path(), root.path())
                .unwrap()
                .with_prompt_provider(provider.clone())
                .unwrap();
            run(&reload, &session).await;
            assert_eq!(state.requests.lock().unwrap().len(), before + 2);
        }
        assert_eq!(
            std::fs::read_to_string(root.path().join("a.txt")).unwrap(),
            "workspace evidence"
        );
        *state.reply.lock().unwrap() = (200, "{\"ok\":false,\"reason\":\"denied\"}".into());
        let out = run(&runner, "denied").await;
        assert!(out.blocked && out.prevent_continuation && !out.allow_permission);
        server.abort();
    }
}

#[tokio::test]
async fn limits_invalid_timeout_cancel_and_close_do_not_consume_once() {
    let (url, state, server) = fixture().await;
    let root = tempfile::tempdir().unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    let runner = Arc::new(runner(
        root.path(),
        provider(&url, "openai"),
        json!({"once":true,"statusMessage":"Checking"}),
    ));
    let mut progress = crate::services::hook_progress::subscribe(&session);
    for reply in ["{}", "{\"ok\":\"true\"}", "{\"ok\":true,\"reason\":null}"] {
        *state.reply.lock().unwrap() = (200, reply.into());
        let before = state.requests.lock().unwrap().len();
        let out = run(&runner, &session).await;
        assert!(!out.blocked && !out.allow_permission && out.warnings.is_empty());
        assert_eq!(state.requests.lock().unwrap().len(), before + 1);
    }
    state.reads.store(100, Ordering::Relaxed);
    *state.tool.lock().unwrap() = "Bash".into();
    let before = state.requests.lock().unwrap().len();
    let mut bounded = HookRunner::load(root.path(), root.path())
        .unwrap()
        .with_prompt_provider(provider(&url, "openai"))
        .unwrap();
    bounded.hooks[0].timeout = 30;
    let out = run(&bounded, &session).await;
    assert!(!out.blocked && !out.allow_permission);
    assert_eq!(state.requests.lock().unwrap().len(), before + MAX_TURNS);
    state.reads.store(0, Ordering::Relaxed);
    state.delay.store(3000, Ordering::Relaxed);
    let out = run(&runner, &session).await;
    assert!(!out.allow_permission && out.warnings.is_empty());
    assert!(progress.snapshot().is_empty());
    for close in [false, true] {
        let worker = runner.clone();
        let id = session.clone();
        let task = tokio::spawn(async move { run(&worker, &id).await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while progress.snapshot().is_empty() {
                progress.changed().await;
            }
        })
        .await
        .unwrap();
        if close {
            let data = tempfile::tempdir().unwrap();
            let context = crate::services::ServiceContext::open(
                crate::runtime::RuntimePaths::new(data.path().into()).unwrap(),
                Arc::new(crate::services::EnvironmentCredentials),
            )
            .unwrap();
            crate::services::session_hooks::end(
                &context,
                root.path(),
                &session,
                "test",
                Duration::from_secs(1),
            )
            .await;
            let out = task.await.unwrap();
            assert!(!out.allow_permission && out.warnings.is_empty());
        } else {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        }
        assert!(progress.snapshot().is_empty());
        assert!(!RUNNING
            .lock()
            .unwrap()
            .iter()
            .any(|(id, _, _)| id == &session));
    }
    state.delay.store(0, Ordering::Relaxed);
    *state.reply.lock().unwrap() = (200, "{\"ok\":true}".into());
    let before = state.requests.lock().unwrap().len();
    assert!(run(&runner, &session).await.warnings.is_empty());
    assert_eq!(state.requests.lock().unwrap().len(), before + 1);
    server.abort();
}

#[tokio::test]
async fn independent_permissions_reject_outside_and_oversized_files() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret"), "secret").unwrap();
    std::fs::write(root.path().join("big"), vec![b'a'; 65537]).unwrap();
    let context = ReadOnlyContext {
        root: std::fs::canonicalize(root.path()).unwrap(),
    };
    assert!(context
        .call("Read", &json!({"file_path":outside.path().join("secret")}))
        .await
        .unwrap_err()
        .to_string()
        .contains("permission denied"));
    assert!(context
        .call("Read", &json!({"file_path":"big"}))
        .await
        .is_err());
    assert!(context
        .call("Read", &json!({"file_path":"big","command":"echo"}))
        .await
        .is_err());
}

#[test]
fn reject_events_and_unsupported_options() {
    for event in [
        "SessionStart",
        "SessionEnd",
        "Notification",
        "SubagentStart",
        "PreCompact",
        "PostCompact",
        "WorktreeCreate",
        "WorktreeRemove",
        "PermissionDenied",
        "Unknown",
    ] {
        let root = tempfile::tempdir().unwrap();
        crate::data_layout::write_config_file(
            crate::data_layout::DataLayout::from_root(&root.path()).config_file("settings.json"),
            json!({"hooks":{event:[{"hooks":[{"type":"agent","prompt":"verify"}]}]}}).to_string(),
        )
        .unwrap();
        assert!(
            HookRunner::load(root.path(), root.path()).is_err(),
            "{event}"
        );
    }
    for extra in [
        json!({"async":false}),
        json!({"shell":"bash"}),
        json!({"maxTurns":3}),
        json!({"tools":["Bash"]}),
        json!({"timeout":0}),
        json!({"timeout":0.5}),
    ] {
        let root = tempfile::tempdir().unwrap();
        let mut hook = json!({"type":"agent","prompt":"verify"});
        hook.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        crate::data_layout::write_config_file(
            crate::data_layout::DataLayout::from_root(&root.path()).config_file("settings.json"),
            json!({"hooks":{"Stop":[{"hooks":[hook]}]}}).to_string(),
        )
        .unwrap();
        assert!(
            HookRunner::load(root.path(), root.path()).is_err(),
            "{extra}"
        );
    }
}

#[tokio::test]
async fn token_context_and_transport_failures_are_bounded() {
    let (url, state, server) = fixture().await;
    let root = tempfile::tempdir().unwrap();
    let mut selected = provider(&url, "openai");
    selected.model.max_tokens = Some(64);
    let hook = runner(root.path(), selected.clone(), json!({"once":true}));
    *state.reply.lock().unwrap() = (500, "{}".into());
    let out = run(&hook, "transport").await;
    assert!(!out.allow_permission && !out.blocked && !out.warnings.is_empty());
    assert_eq!(
        state.requests.lock().unwrap().len(),
        1,
        "No transport retries"
    );
    *state.reply.lock().unwrap() = (200, "{\"ok\":true}".into());
    assert!(run(&hook, "transport").await.warnings.is_empty());
    assert_eq!(state.requests.lock().unwrap()[1]["max_tokens"], 64);
    selected.model.context_window = Some(1);
    let bounded = runner(root.path(), selected, json!({}));
    let out = run(&bounded, "context").await;
    assert!(!out.allow_permission && out.warnings[0].contains("context budget"));
    assert_eq!(
        state.requests.lock().unwrap().len(),
        2,
        "Oversized input must not reach provider"
    );
    server.abort();
}
