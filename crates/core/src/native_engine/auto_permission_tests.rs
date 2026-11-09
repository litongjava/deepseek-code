use super::*;
use crate::permissions::{
    auto_config::{self, AutoConfig, ClassifierSelection},
    rules::PermissionAction,
    PermissionMode,
};
use axum::{routing::post, Json, Router};

fn response(decision: &str) -> Value {
    json!({"id":"classifier","object":"chat.completion","created":0,"model":"classifier","choices":[{"index":0,"finish_reason":"stop","message":{"role":"assistant","content":json!({"decision":decision,"reason_code":if decision=="allow"{"authorized"}else{"outside_scope"}}).to_string()}}],"usage":{"prompt_tokens":7,"completion_tokens":3,"total_tokens":10}})
}

fn executor(
    root: &std::path::Path,
    endpoint: &str,
) -> (
    ToolLoopExecutor,
    mpsc::Receiver<EngineEvent>,
    Arc<PermissionManager>,
) {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let context = crate::services::session_messages::context(&root.join("data")).unwrap();
    auto_config::save(
        &context,
        &AutoConfig {
            enabled: true,
            classifier: Some(ClassifierSelection::Current),
            timeout_ms: 1000,
            ..Default::default()
        },
    )
    .unwrap();
    context
        .db
        .with_conn(|conn| {
            conn.execute(
                "INSERT INTO conversations(id,created_at,updated_at) VALUES('auto-session','','')",
                [],
            )
        })
        .unwrap()
        .unwrap();
    crate::services::permission_authority::record_user(
        &context.db,
        "auto-session",
        &json!("Write the requested fixture file"),
    )
    .unwrap();
    let provider = ResolvedProvider { provider:serde_json::from_value(json!({"id":"fixture","name":"Fixture","base_url":endpoint,"api_key":"fixture-only","api_format":"openai","models":[],"enabled":true})).unwrap(),model:serde_json::from_value(json!({"id":"classifier","name":"Fixture","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap() };
    let manager = crate::runtime::default_permissions();
    manager.set_mode(PermissionMode::Auto);
    let (tx, rx) = mpsc::channel(64);
    let executor = ToolLoopExecutor::new(
        provider,
        vec![json!({"role":"user","content":"Write the requested fixture file"})],
        None,
        512,
        tx,
        root.to_string_lossy().into_owned(),
        ReasoningMode::Standard,
    )
    .with_conv_id("auto-session".into())
    .with_search_config_path(root.join("data/config.json"))
    .with_permission_manager(manager.clone());
    (executor, rx, manager)
}

#[tokio::test]
async fn auto_executes_only_authorized_actual_writes_and_never_creates_unresolved_waiters() {
    for decision in ["allow", "deny"] {
        let root = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let calls = Arc::new(AtomicUsize::new(0));
        let requests = calls.clone();
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |Json(body): Json<Value>| {
                let requests = requests.clone();
                async move {
                    requests.fetch_add(1, Ordering::SeqCst);
                    assert!(body.get("tools").is_none());
                    let payload: Value =
                        serde_json::from_str(body["messages"][1]["content"].as_str().unwrap())
                            .unwrap();
                    assert_eq!(payload["proposed_call"]["input"]["content"], "ACTUAL_INPUT");
                    Json(response(decision))
                }
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let (mut executor, mut events, manager) = executor(root.path(), &endpoint);
        let (_, output, failed) = executor
            .execute_tool_call(
                "Write",
                &json!({"file_path":".bashrc","content":"ACTUAL_INPUT"}),
                "write",
            )
            .await;
        assert_eq!(failed, decision == "deny", "{output}");
        assert_eq!(root.path().join(".bashrc").exists(), decision == "allow");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        manager.set_session_rules(
            "auto-session",
            HashMap::from([("Write".into(), PermissionAction::Deny)]),
        );
        let (_, output, failed) = executor
            .execute_tool_call(
                "Write",
                &json!({"file_path":"explicit-deny.txt","content":"ACTUAL_INPUT"}),
                "deny",
            )
            .await;
        assert!(failed, "{output}");
        assert!(!root.path().join("explicit-deny.txt").exists());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(executor.answer_waiters.lock().await.is_empty());
        let mut usage = false;
        while let Ok(event) = events.try_recv() {
            match event {
                EngineEvent::ToolPermission { .. } => {
                    panic!("unresolved auto must not request permission")
                }
                EngineEvent::Usage(value) => {
                    assert_eq!(value["auto_permission"]["stats"]["input_tokens"], 7);
                    usage = true;
                }
                _ => {}
            }
        }
        assert!(usage);
        server.abort();
    }
}

#[tokio::test]
async fn auto_discards_allow_after_rule_mode_or_config_changes_and_cancellation() {
    for mutation in [
        "rule",
        "runtime",
        "callback",
        "mode",
        "config",
        "provider",
        "authority",
        "cancel",
    ] {
        let root = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let seen = started.clone();
        let gate = release.clone();
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move || {
                let seen = seen.clone();
                let gate = gate.clone();
                async move {
                    seen.notify_one();
                    gate.notified().await;
                    Json(response("allow"))
                }
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let (mut executor, _events, manager) = executor(root.path(), &endpoint);
        if mutation == "provider" {
            let context =
                crate::services::session_messages::context(&root.path().join("data")).unwrap();
            let mut provider = executor.provider.provider.clone();
            provider.models = vec![executor.provider.model.clone()];
            std::fs::write(
                root.path().join("data/providers.json"),
                serde_json::to_vec(&vec![provider]).unwrap(),
            )
            .unwrap();
            let mut config = auto_config::load(&context).unwrap();
            config.classifier = Some(ClassifierSelection::Named {
                provider_id: "fixture".into(),
                model_id: "classifier".into(),
            });
            auto_config::save(&context, &config).unwrap();
        }
        let waiters = executor.answer_waiters.clone();
        let pending = tokio::spawn(async move {
            executor
                .execute_tool_call(
                    "Write",
                    &json!({"file_path":".bashrc","content":"ACTUAL_INPUT"}),
                    "write",
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(3), started.notified())
            .await
            .unwrap();
        match mutation {
            "rule" => manager.set_session_rules(
                "auto-session",
                HashMap::from([("Write".into(), PermissionAction::Deny)]),
            ),
            "runtime" => manager.set_runtime_rules(
                "auto-session",
                vec![HashMap::from([("Write".into(), PermissionAction::Deny)])],
            ),
            "callback" => manager.set_always_allow_checker(Box::new(|_, _| false)),
            "provider" => {
                std::fs::write(root.path().join("data/providers.json"), "[]").unwrap();
            }
            "mode" => {
                manager.set_mode(PermissionMode::AskPermissions);
                manager.set_mode(PermissionMode::Auto);
            }
            "config" => {
                let context =
                    crate::services::session_messages::context(&root.path().join("data")).unwrap();
                auto_config::save(&context, &AutoConfig::default()).unwrap();
            }
            "authority" => {
                let context =
                    crate::services::session_messages::context(&root.path().join("data")).unwrap();
                crate::services::permission_authority::record_user(
                    &context.db,
                    "auto-session",
                    &json!("Do not write any files"),
                )
                .unwrap();
            }
            "cancel" => pending.abort(),
            _ => unreachable!(),
        }
        release.notify_one();
        if mutation == "cancel" {
            assert!(pending.await.is_err());
            let stats = manager.auto_stats("auto-session");
            assert_eq!(stats.requests, 1);
            assert_eq!(stats.unknown_usage_requests, 1);
        } else {
            let (_, output, failed) = pending.await.unwrap();
            assert!(failed, "{output}");
            assert!(output.contains("policy changed"), "{output}");
        }
        assert!(!root.path().join(".bashrc").exists());
        assert!(waiters.lock().await.is_empty());
        server.abort();
    }
}

#[tokio::test]
async fn auto_failure_fallback_asks_interactive_hosts_even_with_legacy_deny_policy() {
    use crate::permissions::auto_config::FailurePolicy;
    for manual in [false, true] {
        for interactive in [false, true] {
            for failure in ["transport", "invalid", "timeout", "configuration"] {
                let root = tempfile::tempdir().unwrap();
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let endpoint = format!("http://{}", listener.local_addr().unwrap());
                let calls = Arc::new(AtomicUsize::new(0));
                let requests = calls.clone();
                let app = Router::new().route(
                    "/v1/chat/completions",
                    post(move || {
                        let requests = requests.clone();
                        async move {
                            requests.fetch_add(1, Ordering::SeqCst);
                            if failure == "timeout" {
                                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                            }
                            (
                                if failure == "transport" {
                                    axum::http::StatusCode::SERVICE_UNAVAILABLE
                                } else {
                                    axum::http::StatusCode::OK
                                },
                                Json(response("invalid")),
                            )
                        }
                    }),
                );
                let server = tokio::spawn(async move {
                    axum::serve(listener, app).await.unwrap();
                });
                let (executor, _events, manager) = executor(root.path(), &endpoint);
                let context =
                    crate::services::session_messages::context(&root.path().join("data")).unwrap();
                let mut config = auto_config::load(&context).unwrap();
                config.failure_policy = if manual {
                    FailurePolicy::Manual
                } else {
                    FailurePolicy::Deny
                };
                config.timeout_ms = 100;
                if failure == "configuration" {
                    config.classifier = Some(ClassifierSelection::Named {
                        provider_id: "missing".into(),
                        model_id: "missing".into(),
                    });
                }
                auto_config::save(&context, &config).unwrap();
                let tool = crate::permissions::PermissionContext {
                    mcp_read_only: false,
                    tool_name: "Write".into(),
                    tool_input: json!({"file_path":".bashrc","content":"fixture"}),
                    conversation_id: "auto-session".into(),
                    user_id: None,
                    workspace_path: Some(root.path().to_string_lossy().into_owned()),
                };
                let authorization = crate::native_engine::auto_permissions::authorize(
                    &context,
                    &manager,
                    &tool,
                    &executor.provider,
                    &[],
                    interactive,
                )
                .await;
                assert_eq!(
                    matches!(
                        authorization.result,
                        crate::permissions::PermissionResult::RequiresConfirmation(_)
                    ),
                    interactive
                );
                let attempts = if failure == "configuration" { 0 } else { 1 };
                assert_eq!(calls.load(Ordering::SeqCst), attempts);
                assert_eq!(manager.auto_stats("auto-session").requests, attempts as u64);
                let mut oversized = tool.clone();
                oversized.tool_input = json!({"file_path":".bashrc","content": "x".repeat(40_000)});
                let limited = crate::native_engine::auto_permissions::authorize(
                    &context,
                    &manager,
                    &oversized,
                    &executor.provider,
                    &[],
                    interactive,
                )
                .await;
                let message = match limited.result {
                    crate::permissions::PermissionResult::RequiresConfirmation(message) => {
                        assert!(interactive);
                        assert!(message.contains("requesting manual permission"));
                        message
                    }
                    crate::permissions::PermissionResult::Denied(message) => {
                        assert!(!interactive);
                        assert!(message.contains("no manual approval host"));
                        message
                    }
                    _ => panic!("Oversized classifier input must never grant permission"),
                };
                assert!(message.contains(if failure == "configuration" {
                    "configuration"
                } else {
                    "InputLimit"
                }));
                assert_eq!(manager.auto_stats("auto-session").denied_total, 0);
                assert_eq!(calls.load(Ordering::SeqCst), attempts);
                manager.set_session_rules(
                    "auto-session",
                    HashMap::from([("Write".into(), PermissionAction::Confirm)]),
                );
                let explicit = crate::native_engine::auto_permissions::authorize(
                    &context,
                    &manager,
                    &tool,
                    &executor.provider,
                    &[],
                    interactive,
                )
                .await;
                assert!(matches!(
                    explicit.result,
                    crate::permissions::PermissionResult::RequiresConfirmation(_)
                ));
                assert_eq!(
                    calls.load(Ordering::SeqCst),
                    attempts,
                    "explicit asks must never invoke classifier"
                );
                server.abort();
            }
        }
    }
}

#[tokio::test]
async fn auto_denials_do_not_limit_requests_and_success_resets_only_consecutive_count() {
    let root = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let requests = calls.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move || {
            let requests = requests.clone();
            async move {
                let n = requests.fetch_add(1, Ordering::SeqCst);
                Json(response(if n < 21 { "deny" } else { "allow" }))
            }
        }),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let (mut executor, _events, manager) = executor(root.path(), &endpoint);
    for n in 0..21 {
        let (_, _, failed) = executor
            .execute_tool_call(
                "Write",
                &json!({"file_path":".bashrc","content":"denied"}),
                &format!("write-{n}"),
            )
            .await;
        assert!(failed);
    }
    assert_eq!(calls.load(Ordering::SeqCst), 21);
    assert_eq!(manager.auto_stats("auto-session").denied_consecutive, 21);
    assert!(!root.path().join(".bashrc").exists());
    let (_, output, failed) = executor
        .execute_tool_call(
            "Write",
            &json!({"file_path":".profile","content":"approved"}),
            "approved-after-denials",
        )
        .await;
    assert!(!failed, "{output}");
    assert!(root.path().join(".profile").exists());
    assert_eq!(calls.load(Ordering::SeqCst), 22);
    // The outer execution loop records successful tools after dispatch.
    manager.record_tool_success("auto-session");
    let stats = manager.auto_stats("auto-session");
    assert_eq!(stats.denied_consecutive, 0);
    assert_eq!(stats.denied_total, 21);
    assert_eq!(manager.auto_stats("other-session").denied_total, 0);
    server.abort();
}

#[tokio::test]
async fn auto_input_limit_never_counts_a_network_attempt() {
    let root = tempfile::tempdir().unwrap();
    let (mut executor, _events, manager) = executor(root.path(), "http://127.0.0.1:1");
    let context = crate::services::session_messages::context(&root.path().join("data")).unwrap();
    let mut config = auto_config::load(&context).unwrap();
    config.max_input_tokens = 256;
    auto_config::save(&context, &config).unwrap();
    let (_, output, failed) = executor
        .execute_tool_call(
            "Write",
            &json!({"file_path":".bashrc","content":"X".repeat(8192)}),
            "write",
        )
        .await;
    assert!(failed && output.contains("InputLimit"), "{output}");
    let stats = manager.auto_stats("auto-session");
    assert_eq!(stats.requests, 0);
    assert_eq!(stats.unknown_usage_requests, 0);
    assert_eq!(stats.denied_total, 0);
    assert!(!root.path().join(".bashrc").exists());
}

#[tokio::test]
async fn auto_rejected_classification_asks_host_and_respects_answer_for_external_write() {
    for approve in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let file = outside.path().join(".bashrc");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route(
            "/v1/chat/completions",
            post(|| async { Json(response("deny")) }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let (executor, mut events, _manager) = executor(root.path(), &endpoint);
        let mut executor = executor.with_auto_manual_interactive(true);
        let waiters = executor.answer_waiters.clone();
        let input = json!({"file_path":file,"content":"approved"});
        let pending = tokio::spawn(async move {
            executor
                .execute_tool_call("Write", &input, "external-write")
                .await
        });
        let request = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if let EngineEvent::ToolPermission { request_id, .. } = events.recv().await.unwrap()
                {
                    break request_id;
                }
            }
        })
        .await
        .unwrap();
        waiters
            .lock()
            .await
            .remove(&request)
            .unwrap()
            .send(if approve { "yes" } else { "no" }.into())
            .unwrap();
        let (_, output, failed) = pending.await.unwrap();
        assert_eq!(failed, !approve, "{output}");
        assert_eq!(file.exists(), approve);
        if approve {
            assert_eq!(std::fs::read_to_string(&file).unwrap(), "approved");
        }
        server.abort();
    }
}

#[tokio::test]
async fn auto_allows_exact_external_write_without_manual_host() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let file = outside.path().join("new/sub/file.txt");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().route(
        "/v1/chat/completions",
        post(|| async { Json(response("allow")) }),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let (mut executor, _events, _manager) = executor(root.path(), &endpoint);
    let (_, output, failed) = executor
        .execute_tool_call(
            "Write",
            &json!({"file_path":file,"content":"approved"}),
            "external-write",
        )
        .await;
    assert!(!failed, "{output}");
    assert_eq!(std::fs::read_to_string(file).unwrap(), "approved");
    server.abort();
}

#[tokio::test]
async fn auto_missing_authority_and_configuration_fall_back_without_weakening_deny() {
    use crate::permissions::PermissionResult;
    for interactive in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (executor, _events, manager) = executor(root.path(), "http://127.0.0.1:1");
        let mut executor = executor.with_auto_manual_interactive(interactive);
        executor.conv_id = Some("no-verified-history".into());
        let input = json!({"file_path":".bashrc","content":"test"});
        let result = executor
            .check_permission("Write", &input, "no-history")
            .await;
        assert_eq!(
            matches!(result, PermissionResult::RequiresConfirmation(_)),
            interactive
        );
        if !interactive {
            assert!(matches!(result, PermissionResult::Denied(_)));
        }
        executor.search_config_path = None;
        let result = executor
            .check_permission("Write", &input, "no-config")
            .await;
        assert_eq!(
            matches!(result, PermissionResult::RequiresConfirmation(_)),
            interactive
        );
        manager.set_session_rules(
            "no-verified-history",
            HashMap::from([("Write".into(), PermissionAction::Deny)]),
        );
        assert!(matches!(
            executor.check_permission("Write", &input, "deny").await,
            PermissionResult::Denied(_)
        ));
    }
}

#[tokio::test]
async fn auto_dangerous_shell_keyword_asks_then_executes_only_approved_command() {
    let root = tempfile::tempdir().unwrap();
    let (executor, mut events, _manager) = executor(root.path(), "http://127.0.0.1:1");
    let mut executor = executor.with_auto_manual_interactive(true);
    let waiters = executor.answer_waiters.clone();
    // Print the keyword, never execute a destructive command.
    let input = if cfg!(windows) {
        json!({"shell":"powershell","command":"Invoke-Expression \"Write-Output 'shutdown /s /t 0'\""})
    } else {
        json!({"shell":"sh","command":"eval \"printf '%s' 'shutdown /s /t 0'\""})
    };
    let pending =
        tokio::spawn(async move { executor.execute_tool_call("Bash", &input, "keyword").await });
    let EngineEvent::ToolPermission { request_id, .. } = events.recv().await.unwrap() else {
        panic!("expected manual approval without classifier request")
    };
    waiters
        .lock()
        .await
        .remove(&request_id)
        .unwrap()
        .send("yes".into())
        .unwrap();
    let (_, output, failed) = pending.await.unwrap();
    assert!(!failed, "{output}");
    assert!(output.contains("shutdown /s /t 0"));
}

#[tokio::test]
async fn ordinary_auto_edits_need_neither_classifier_nor_manual_host() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let (mut executor, mut events, manager) = executor(root.path(), "http://127.0.0.1:1");
    // MultiEdit and NotebookEdit load on demand by default; this test drives
    // them directly, so it turns both on for its own data directory.
    crate::services::customization::save(
        &root.path().join("data"),
        &crate::services::customization::Customization {
            tool_loading: ["MultiEdit", "NotebookEdit"]
                .into_iter()
                .map(|name| {
                    (
                        name.to_owned(),
                        crate::services::customization::ToolLoading {
                            enabled: true,
                            on_demand: true,
                        },
                    )
                })
                .collect(),
            ..Default::default()
        },
        &[],
    )
    .unwrap();
    let notebook = outside.path().join("normal.ipynb");
    std::fs::write(
        &notebook,
        json!({"nbformat":4,"nbformat_minor":5,"metadata":{},"cells":[]}).to_string(),
    )
    .unwrap();
    for (tool, input) in [
        (
            "Write",
            json!({"file_path":outside.path().join("normal.txt"),"content":"before"}),
        ),
        (
            "Edit",
            json!({"file_path":outside.path().join("normal.txt"),"old_string":"before","new_string":"after"}),
        ),
        (
            "MultiEdit",
            json!({"file_path":outside.path().join("normal.txt"),"edits":[{"old_string":"after","new_string":"final"}]}),
        ),
        ("Read", json!({"file_path":notebook})),
        (
            "NotebookEdit",
            json!({"notebook_path":notebook,"edit_mode":"insert","cell_type":"markdown","new_source":"cell"}),
        ),
    ] {
        let (_, output, failed) = executor.execute_tool_call(tool, &input, tool).await;
        assert!(!failed, "{output}");
    }
    assert_eq!(
        std::fs::read_to_string(outside.path().join("normal.txt")).unwrap(),
        "final"
    );
    assert_eq!(manager.auto_stats("auto-session").requests, 0);
    while let Ok(event) = events.try_recv() {
        assert!(!matches!(event, EngineEvent::ToolPermission { .. }));
    }
}
