use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

#[tokio::test]
async fn typed_approval_validates_before_committing_and_uses_effective_input() {
    crate::config::OrchestrationConfig::init_global().unwrap();
    for case in [
        "replacement",
        "empty",
        "invalid",
        "outside",
        "interrupt",
        "plan",
        "deny",
    ] {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let provider = ResolvedProvider {
            provider: serde_json::from_value(json!({"id":"fixture","name":"fixture","base_url":"http://127.0.0.1:1","api_key":"unused","api_format":"openai","models":[],"enabled":true})).unwrap(),
            model: serde_json::from_value(json!({"id":"fixture","name":"fixture","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap(),
        };
        let (tx, mut events) = mpsc::channel(16);
        let mut executor = ToolLoopExecutor::new(
            provider,
            vec![],
            None,
            128,
            tx,
            root.path().to_string_lossy().into_owned(),
            ReasoningMode::Standard,
        );
        let manager = Arc::new(crate::permissions::PermissionManager::new(Arc::new(
            crate::permissions::AuditLogger::new(16),
        )));
        manager.set_mode(crate::permissions::PermissionMode::AskPermissions);
        executor = executor.with_permission_manager(manager.clone());
        let mailbox = executor.permission_mailbox.clone();
        let waiters = executor.get_answer_waiters();
        let task = tokio::spawn(async move {
            let result = executor
                .execute_ask_user_confirmation(
                    "Write",
                    &json!({"file_path":"original.txt","content":"original"}),
                    "write-1",
                    "approve",
                )
                .await;
            (
                result,
                executor.ensure_permission_continuation().is_err(),
                executor.approved_call.is_none(),
            )
        });
        let EngineEvent::ToolPermission { request_id, .. } = events.recv().await.unwrap() else {
            panic!()
        };
        if case == "plan" {
            manager.set_mode(crate::permissions::PermissionMode::PlanMode);
        }
        if case == "deny" {
            manager.set_tool_permission(
                "Write",
                crate::permissions::ToolPermission {
                    tool_name: "Write".into(),
                    allowed: false,
                    requires_confirmation: false,
                    scope: crate::permissions::PermissionScope::Global,
                    level: crate::permissions::PermissionLevel::Execute,
                },
            );
        }
        let committed = Arc::new(AtomicBool::new(false));
        let flag = committed.clone();
        let updated = match case {
            "replacement" => json!({"file_path":"replacement.txt","content":"replacement"}),
            "empty" | "plan" | "deny" => json!({}),
            "outside" => {
                json!({"file_path":outside.path().join("outside.txt"),"content":"outside"})
            }
            _ => json!({"file_path":"invalid.txt"}),
        };
        let response = if case == "interrupt" {
            PermissionResponse::Deny {
                message: "STOP_SENTINEL".into(),
                interrupt: true,
            }
        } else {
            PermissionResponse::Allow {
                updated_input: Some(updated),
                commit: Some(Box::new(move || {
                    flag.store(true, Ordering::SeqCst);
                    Ok(())
                })),
            }
        };
        crate::native_engine::permission_response::respond(&mailbox, &request_id, response)
            .await
            .unwrap();
        let ((actual, output, failed), interrupted, cleared) = task.await.unwrap();
        let allowed = matches!(case, "replacement" | "empty");
        assert_eq!(failed, !allowed, "{case}: {output}");
        assert_eq!(committed.load(Ordering::SeqCst), allowed);
        assert_eq!(interrupted, case == "interrupt");
        assert!(cleared);
        if allowed {
            let context = crate::permissions::PermissionContext {
                tool_name: "Write".into(),
                tool_input: json!({"file_path":"another.txt","content":"no grant"}),
                conversation_id: String::new(),
                user_id: None,
                workspace_path: Some(root.path().to_string_lossy().into_owned()),
            };
            assert!(
                matches!(
                    manager.check_permission(&context),
                    PermissionResult::RequiresConfirmation(_)
                ),
                "one-shot approval must not grant other calls"
            );
        }
        assert!(mailbox.lock().await.is_empty());
        assert!(waiters.lock().await.is_empty());
        assert!(!outside.path().join("outside.txt").exists());
        assert_eq!(root.path().join("original.txt").exists(), case == "empty");
        if case == "replacement" {
            assert_eq!(actual["file_path"], "replacement.txt");
            assert_eq!(
                std::fs::read_to_string(root.path().join("replacement.txt")).unwrap(),
                "replacement"
            );
        }
        assert!(crate::native_engine::permission_response::respond(
            &mailbox,
            &request_id,
            PermissionResponse::Deny {
                message: "late".into(),
                interrupt: false
            }
        )
        .await
        .is_err());
    }
}

#[test]
fn approved_paths_resolve_symlinks_before_parent_traversal() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let cwd = root.path().to_str().unwrap();
    assert!(validate_workspace_path("missing/../../outside.txt", cwd, &[]).is_err());
    // Windows resolves missing/.. to the existing parent; Unix rejects the unresolved ancestor.
    #[cfg(unix)]
    assert!(validate_workspace_path("missing/../inside.txt", cwd, &[]).is_err());
    assert!(validate_workspace_path("missing/nested/file.txt", cwd, &[]).is_ok());
    std::fs::create_dir(outside.path().join("nested")).unwrap();
    let link = root.path().join("link");
    #[cfg(windows)]
    assert!(std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&link)
        .arg(outside.path().join("nested"))
        .output()
        .unwrap()
        .status
        .success());
    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.path().join("nested"), &link).unwrap();
    #[cfg(unix)]
    assert!(validate_workspace_path("link/../escape.txt", cwd, &[]).is_err());
    assert!(validate_workspace_path("link/../../escape.txt", cwd, &[]).is_err());
    assert!(validate_workspace_path("link/new/file.txt", cwd, &[]).is_err());
    assert!(
        validate_workspace_path("link/new/file.txt", cwd, &[outside.path().to_path_buf()]).is_ok()
    );
}

fn hook_executor(
    root: &std::path::Path,
    event: &str,
    responses: Vec<Value>,
) -> (
    ToolLoopExecutor,
    mpsc::Receiver<EngineEvent>,
    Arc<crate::permissions::PermissionManager>,
) {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let data = root.join("data");
    std::fs::create_dir_all(&data).unwrap();
    let hooks: Vec<_> = responses.into_iter().map(|response| {
        let command = if cfg!(windows) { format!("[IO.File]::WriteAllText('hook-ran','yes'); [Console]::Out.Write('{}')", response.to_string().replace('\'', "''")) }
            else { format!("printf yes > hook-ran; printf '%s' '{}'", response.to_string().replace('\'', "'\\''")) };
        json!({"type":"command","shell":if cfg!(windows) {"powershell"} else {"sh"},"command":command})
    }).collect();
    std::fs::write(
        data.join("settings.json"),
        json!({"hooks":{event:[{"matcher":"Write","hooks":hooks}]}}).to_string(),
    )
    .unwrap();
    let provider = ResolvedProvider {
        provider: serde_json::from_value(json!({"id":"fixture","name":"fixture","base_url":"http://127.0.0.1:1","api_key":"unused","api_format":"openai","models":[],"enabled":true})).unwrap(),
        model: serde_json::from_value(json!({"id":"fixture","name":"fixture","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap(),
    };
    let (tx, rx) = mpsc::channel(16);
    let manager = Arc::new(crate::permissions::PermissionManager::new(Arc::new(
        crate::permissions::AuditLogger::new(16),
    )));
    manager.set_mode(crate::permissions::PermissionMode::AskPermissions);
    let executor = ToolLoopExecutor::new(
        provider,
        vec![],
        None,
        128,
        tx,
        root.to_string_lossy().into_owned(),
        ReasoningMode::Standard,
    )
    .with_permission_manager(manager.clone())
    .with_permission_host(false)
    .with_hooks(crate::services::tool_hooks::HookRunner::load(&data, root).unwrap());
    (executor, rx, manager)
}

#[tokio::test]
async fn owned_child_permission_hooks_keep_exact_invocation_and_session_on_both_protocols() {
    use axum::{routing::post, Json, Router};
    for format in ["openai", "anthropic"] {
        let root = tempfile::tempdir().unwrap();
        let (mut executor, _events, _) = hook_executor(root.path(), "PermissionRequest", vec![]);
        let data = root.path().join("data");
        let paths = crate::runtime::RuntimePaths::new(data.clone()).unwrap();
        let sessions = crate::runtime::SessionStore::new(paths.open_database().unwrap());
        let parent = sessions.create(root.path(), "fixture", "fixture").unwrap();
        let decision = json!({"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow","updatedInput":{"file_path":"child-approved.txt","content":"CHILD_HOOK_ALLOWED"}}}});
        let command = if cfg!(windows) {
            format!("[IO.File]::WriteAllText('permission-payload.json',[Console]::In.ReadToEnd()); [Console]::Out.Write('{}')",decision)
        } else {
            format!("cat > permission-payload.json; printf '%s' '{}'", decision)
        };
        std::fs::write(data.join("settings.json"),json!({"hooks":{"PermissionRequest":[{"matcher":"Write","hooks":[{"type":"command","shell":if cfg!(windows){"powershell"}else{"sh"},"command":command}]}]}}).to_string()).unwrap();
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let requests = count.clone();
        let app = Router::new().route(if format=="openai" {"/v1/chat/completions"}else{"/v1/messages"},post(move |Json(_body):Json<Value>| {
            let index=requests.fetch_add(1,Ordering::SeqCst);
            async move {
                let args=json!({"file_path":"child-original.txt","content":"ORIGINAL"});
                let frames=if format=="openai" {
                    vec![json!({"choices":[{"index":0,"delta":if index==0 {json!({"tool_calls":[{"index":0,"id":"child-exact-id","type":"function","function":{"name":"Write","arguments":args.to_string()}}]})}else{json!({"content":"CHILD_COMPLETE"})},"finish_reason":if index==0{"tool_calls"}else{"stop"}}]})]
                } else {
                    let mut frames=vec![json!({"type":"message_start","message":{"id":"fixture","model":"fixture","usage":{"input_tokens":1}}})];
                    if index==0 {frames.extend([json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"child-exact-id","name":"Write","input":{}}}),json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":args.to_string()}})]);}else{frames.extend([json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"CHILD_COMPLETE"}})]);}
                    frames.extend([json!({"type":"content_block_stop","index":0}),json!({"type":"message_delta","delta":{"stop_reason":if index==0{"tool_use"}else{"end_turn"}},"usage":{"output_tokens":1}}),json!({"type":"message_stop"})]);frames
                };
                let mut stream=frames.into_iter().map(|frame|format!("data: {frame}\n\n")).collect::<String>();if format=="openai" {stream.push_str("data: [DONE]\n\n");}
                ([("content-type","text/event-stream")],stream)
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        executor.provider.provider.base_url = format!("http://{}", listener.local_addr().unwrap());
        executor.provider.provider.api_format = serde_json::from_value(json!(format)).unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        executor = executor
            .with_conv_id(parent.clone())
            .with_search_config_path(data.join("config.json"))
            .with_hooks(crate::services::tool_hooks::HookRunner::load(&data, root.path()).unwrap());
        let result = tokio::time::timeout(
            Duration::from_secs(15),
            executor.execute_delegated(
                &json!({"prompt":"Write the child fixture","agent_type":"general"}),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result["status"], "completed");
        assert_eq!(count.load(Ordering::SeqCst), 2);
        assert_eq!(
            std::fs::read_to_string(root.path().join("child-approved.txt")).unwrap(),
            "CHILD_HOOK_ALLOWED"
        );
        assert!(!root.path().join("child-original.txt").exists());
        let payload: Value = serde_json::from_slice(
            &std::fs::read(root.path().join("permission-payload.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(payload["tool_use_id"], "child-exact-id");
        assert_eq!(payload["session_id"], result["session_id"]);
        assert_ne!(payload["session_id"], parent);
        server.abort();
    }
}

#[tokio::test]
async fn permission_request_headless_validates_decisions_and_cleans_waiters() {
    for case in [
        "allow",
        "deny",
        "interrupt",
        "invalid",
        "wrong-event",
        "no-decision",
        "dontAsk",
        "plan",
        "static-deny",
    ] {
        let root = tempfile::tempdir().unwrap();
        let mut output = json!({"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow","updatedInput":{"file_path":"replacement.txt","content":"hook replacement"}}}});
        match case {
            "deny" | "interrupt" => {
                output["hookSpecificOutput"]["decision"] =
                    json!({"behavior":"deny","message":"HOOK_DENIED","interrupt":case=="interrupt"})
            }
            "invalid" => {
                output["hookSpecificOutput"]["decision"]["updatedInput"] =
                    json!({"file_path":"invalid.txt"})
            }
            "wrong-event" => output["hookSpecificOutput"]["hookEventName"] = json!("PreToolUse"),
            "no-decision" => output = json!({}),
            _ => {}
        }
        let (mut executor, mut events, manager) =
            hook_executor(root.path(), "PermissionRequest", vec![output]);
        if case == "dontAsk" {
            manager.set_mode(crate::permissions::PermissionMode::DontAsk);
        }
        if case == "plan" {
            manager.set_mode(crate::permissions::PermissionMode::PlanMode);
        }
        if case == "static-deny" {
            manager.set_session_rules(
                "",
                std::collections::HashMap::from([(
                    "Write".into(),
                    crate::permissions::rules::PermissionAction::Deny,
                )]),
            );
        }
        let (_, result, failed) = tokio::time::timeout(
            Duration::from_secs(15),
            executor.execute_tool_call(
                "Write",
                &json!({"file_path":"original.txt","content":"original"}),
                "child-write-exact-id",
            ),
        )
        .await
        .unwrap();
        assert_eq!(failed, case != "allow", "{case}: {result}");
        // Without an approval host an undecided request is denied, not
        // interrupted, so headless runs can finish the turn.
        assert_eq!(
            executor.ensure_permission_continuation().is_err(),
            case == "interrupt"
        );
        assert!(!root.path().join("original.txt").exists());
        assert_eq!(
            root.path().join("replacement.txt").exists(),
            case == "allow"
        );
        assert!(executor.permission_mailbox.lock().await.is_empty());
        assert!(executor.answer_waiters.lock().await.is_empty());
        assert_eq!(
            root.path().join("hook-ran").exists(),
            !matches!(case, "dontAsk" | "plan" | "static-deny")
        );
        while let Ok(event) = events.try_recv() {
            assert!(
                !matches!(event, EngineEvent::ToolPermission { .. }),
                "headless should not publish an unserviceable prompt"
            );
        }
    }
}

#[tokio::test]
async fn pre_tool_allow_is_exact_and_cannot_replace_hard_rules_or_prior_ask() {
    for case in [
        "allow",
        "static-ask",
        "static-deny",
        "plan",
        "dontAsk",
        "ask-then-allow",
        "deny-then-allow",
        "invalid",
        "outside",
        "disabled",
    ] {
        let root = tempfile::tempdir().unwrap();
        let allow = json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow"}});
        let mut hooks = vec![];
        if matches!(case, "ask-then-allow" | "deny-then-allow") {
            hooks.push(json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":if case=="ask-then-allow" {"ask"} else {"deny"}}}));
        }
        hooks.push(allow);
        let (mut executor, _, manager) = hook_executor(root.path(), "PreToolUse", hooks);
        let outside = tempfile::tempdir().unwrap();
        if case == "disabled" {
            executor.allowed_tools = Some(vec!["Read".into()]);
        }
        match case {
            "plan" => manager.set_mode(crate::permissions::PermissionMode::PlanMode),
            "dontAsk" => manager.set_mode(crate::permissions::PermissionMode::DontAsk),
            "static-ask" | "static-deny" => manager.set_session_rules(
                "",
                std::collections::HashMap::from([(
                    "Write".into(),
                    if case == "static-ask" {
                        crate::permissions::rules::PermissionAction::Confirm
                    } else {
                        crate::permissions::rules::PermissionAction::Deny
                    },
                )]),
            ),
            _ => {}
        }
        let input = if case == "invalid" {
            json!({"file_path":"original.txt"})
        } else if case == "outside" {
            json!({"file_path":outside.path().join("outside.txt"),"content":"outside"})
        } else {
            json!({"file_path":"original.txt","content":"original"})
        };
        let (_, result, failed) = tokio::time::timeout(
            Duration::from_secs(15),
            executor.execute_tool_call("Write", &input, "write-1"),
        )
        .await
        .unwrap();
        assert_eq!(
            failed,
            !matches!(case, "allow" | "dontAsk"),
            "{case}: {result}"
        );
        assert_eq!(
            root.path().join("original.txt").exists(),
            matches!(case, "allow" | "dontAsk")
        );
        assert!(executor.approved_call.is_none());
        assert!(!outside.path().join("outside.txt").exists());
        if case == "allow" {
            assert!(matches!(
                manager.check_permission(&crate::permissions::PermissionContext {
                    tool_name: "Write".into(),
                    tool_input: input,
                    conversation_id: String::new(),
                    user_id: None,
                    workspace_path: Some(root.path().to_string_lossy().into_owned())
                }),
                PermissionResult::RequiresConfirmation(_)
            ));
        }
    }
}
