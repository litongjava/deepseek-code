use super::*;
use crate::services::hook_callbacks::{self, Host};
use futures::future::BoxFuture;
use std::sync::Arc;

#[test]
fn replacement_truthiness_matches_reference_and_is_post_only() {
    for value in [Value::Null, json!(false), json!(0), json!(-0.0), json!("")] {
        let outcome = parse_json_output(HookEvent::PostToolUse, json!({"hookSpecificOutput":{"hookEventName":"PostToolUse","updatedMCPToolOutput":value}})).unwrap();
        assert!(outcome.updated_mcp_tool_output.is_none());
    }
    for value in [
        json!([]),
        json!({}),
        json!(true),
        json!(3),
        json!("redacted"),
    ] {
        let outcome = parse_json_output(HookEvent::PostToolUse, json!({"hookSpecificOutput":{"hookEventName":"PostToolUse","updatedMCPToolOutput":value}})).unwrap();
        assert_eq!(outcome.updated_mcp_tool_output, Some(value));
    }
    assert!(parse_json_output(HookEvent::PreToolUse, json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","updatedMCPToolOutput":"not allowed"}})).is_err());
}

#[tokio::test]
async fn delayed_command_tail_cannot_overwrite_newer_callback_output() {
    struct Callback {
        root: PathBuf,
    }
    impl Host for Callback {
        fn invoke<'a>(
            &'a self,
            _: &'a str,
            input: Value,
            _: Option<String>,
        ) -> BoxFuture<'a, Result<Value>> {
            Box::pin(async move {
                assert_eq!(
                    input["tool_response"],
                    json!([{"type":"text","text":"original"}])
                );
                tokio::time::timeout(Duration::from_secs(15), async {
                    while !self.root.join("waiting").exists() {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await?;
                std::fs::write(self.root.join("release"), "yes")?;
                Ok(
                    json!({"hookSpecificOutput":{"hookEventName":"PostToolUse","updatedMCPToolOutput":"callback wins","additionalContext":"kept"}}),
                )
            })
        }
    }
    let root = tempfile::tempdir().unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    let host: Arc<dyn Host> = Arc::new(Callback {
        root: root.path().to_path_buf(),
    });
    let callbacks = hook_callbacks::validate(
        &json!({"PostToolUse":[{"hookCallbackIds":["post"],"timeout":20}]}),
    )
    .unwrap();
    let _guard = hook_callbacks::install(&session, callbacks, host.clone());
    let (shell, first, tail) = if cfg!(windows) {
        ("powershell", "Write-Output '{\"hookSpecificOutput\":{\"hookEventName\":\"PostToolUse\",\"updatedMCPToolOutput\":\"earlier command\"}}'", "[IO.File]::WriteAllText('waiting','yes'); while (!(Test-Path -LiteralPath 'release')) { Start-Sleep -Milliseconds 10 }; Write-Output '{}'")
    } else {
        ("sh", "printf '%s' '{\"hookSpecificOutput\":{\"hookEventName\":\"PostToolUse\",\"updatedMCPToolOutput\":\"earlier command\"}}'", "printf yes > waiting; while [ ! -f release ]; do sleep 0.01; done; printf '{}'")
    };
    let mut runner = HookRunner::default().with_session_callbacks(&session);
    runner.data_dir = root.path().to_path_buf();
    runner.workspace = root.path().to_path_buf();
    runner.hooks = [first, tail]
        .into_iter()
        .map(|command| ActiveHook {
            event: HookEvent::PostToolUse,
            matcher: String::new(),
            command: command.into(),
            http: None,
            prompt: None,
            timeout: 20,
            source: root.path().into(),
            shell: Some(shell.into()),
            if_condition: None,
            once: false,
            asynchronous: false,
            async_rewake: false,
            status_message: None,
        })
        .collect();
    let outcome = runner
        .run(
            HookEvent::PostToolUse,
            &session,
            json!({"tool_name":"fixture","tool_response":[{"type":"text","text":"original"}]}),
        )
        .await
        .unwrap();
    assert_eq!(
        outcome.updated_mcp_tool_output,
        Some(json!("callback wins"))
    );
    assert_eq!(outcome.additional_context, vec!["kept"]);
}

#[tokio::test]
async fn callbacks_observe_original_response_and_last_completion_wins() {
    struct Callback {
        first: tokio::sync::Notify,
    }
    impl Host for Callback {
        fn invoke<'a>(
            &'a self,
            id: &'a str,
            input: Value,
            _: Option<String>,
        ) -> BoxFuture<'a, Result<Value>> {
            Box::pin(async move {
                assert_eq!(input["tool_response"], "original");
                if id == "last" {
                    self.first.notified().await;
                    tokio::task::yield_now().await;
                } else {
                    self.first.notify_one();
                }
                Ok(
                    json!({"hookSpecificOutput":{"hookEventName":"PostToolUse","updatedMCPToolOutput":id}}),
                )
            })
        }
    }
    let session = uuid::Uuid::new_v4().to_string();
    let host: Arc<dyn Host> = Arc::new(Callback {
        first: Default::default(),
    });
    let callbacks =
        hook_callbacks::validate(&json!({"PostToolUse":[{"hookCallbackIds":["last","first"]}]}))
            .unwrap();
    let _guard = hook_callbacks::install(&session, callbacks, host.clone());
    let root = tempfile::tempdir().unwrap();
    let runner = HookRunner::load(root.path(), root.path())
        .unwrap()
        .with_session_callbacks(&session);
    let outcome = runner
        .run(
            HookEvent::PostToolUse,
            &session,
            json!({"tool_name":"fixture","tool_response":"original"}),
        )
        .await
        .unwrap();
    assert_eq!(outcome.updated_mcp_tool_output, Some(json!("last")));
}

#[tokio::test]
async fn initial_message_observation_order_survives_delayed_command_tail() {
    struct Callback {
        root: PathBuf,
    }
    impl Host for Callback {
        fn invoke<'a>(
            &'a self,
            _: &'a str,
            input: Value,
            _: Option<String>,
        ) -> BoxFuture<'a, Result<Value>> {
            Box::pin(async move {
                assert_eq!(
                    input["tool_response"],
                    json!([{"type":"text","text":"original"}])
                );
                tokio::time::timeout(Duration::from_secs(15), async {
                    while !self.root.join("waiting").exists() {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await?;
                std::fs::write(self.root.join("release"), "yes")?;
                Ok(
                    json!({"hookSpecificOutput":{"hookEventName":"SessionStart","initialUserMessage":"callback wins","additionalContext":"kept"}}),
                )
            })
        }
    }
    let root = tempfile::tempdir().unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    let host: Arc<dyn Host> = Arc::new(Callback {
        root: root.path().to_path_buf(),
    });
    let callbacks = hook_callbacks::validate(
        &json!({"SessionStart":[{"hookCallbackIds":["post"],"timeout":20}]}),
    )
    .unwrap();
    let _guard = hook_callbacks::install(&session, callbacks, host.clone());
    let (shell, first, tail) = if cfg!(windows) {
        ("powershell", "Write-Output '{\"hookSpecificOutput\":{\"hookEventName\":\"SessionStart\",\"initialUserMessage\":\"earlier command\"}}'", "[IO.File]::WriteAllText('waiting','yes'); while (!(Test-Path -LiteralPath 'release')) { Start-Sleep -Milliseconds 10 }; Write-Output '{}'")
    } else {
        ("sh", "printf '%s' '{\"hookSpecificOutput\":{\"hookEventName\":\"SessionStart\",\"initialUserMessage\":\"earlier command\"}}'", "printf yes > waiting; while [ ! -f release ]; do sleep 0.01; done; printf '{}'")
    };
    let mut runner = HookRunner::default().with_session_callbacks(&session);
    runner.data_dir = root.path().to_path_buf();
    runner.workspace = root.path().to_path_buf();
    runner.hooks = [first, tail]
        .into_iter()
        .map(|command| ActiveHook {
            event: HookEvent::SessionStart,
            matcher: String::new(),
            command: command.into(),
            http: None,
            prompt: None,
            timeout: 20,
            source: root.path().into(),
            shell: Some(shell.into()),
            if_condition: None,
            once: false,
            asynchronous: false,
            async_rewake: false,
            status_message: None,
        })
        .collect();
    let outcome = runner
        .run(
            HookEvent::SessionStart,
            &session,
            json!({"tool_name":"fixture","tool_response":[{"type":"text","text":"original"}]}),
        )
        .await
        .unwrap();
    assert_eq!(outcome.initial_user_message, Some("callback wins".into()));
    assert_eq!(outcome.additional_context, vec!["kept"]);
}

#[test]
fn initial_message_requires_session_start_string_and_keeps_whitespace() {
    for value in [json!(null), json!(true), json!(1), json!([])] {
        assert!(parse_json_output(HookEvent::SessionStart, json!({"hookSpecificOutput":{"hookEventName":"SessionStart","initialUserMessage":value}})).is_err());
    }
    for event in [
        HookEvent::PreToolUse,
        HookEvent::PermissionDenied,
        HookEvent::Notification,
    ] {
        assert!(parse_json_output(
            event,
            json!({"hookSpecificOutput":{"hookEventName":event.name(),"initialUserMessage":"bad"}})
        )
        .is_err());
    }
    for text in ["", " ", "hello"] {
        let output = parse_json_output(HookEvent::SessionStart, json!({"hookSpecificOutput":{"hookEventName":"SessionStart","initialUserMessage":text}})).unwrap();
        assert_eq!(
            output.initial_user_message.as_deref(),
            (!text.is_empty()).then_some(text)
        );
    }
}
