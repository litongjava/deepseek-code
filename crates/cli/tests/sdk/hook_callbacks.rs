use super::*;

async fn answer(client: &mut Client, request: &Value, output: Value) {
    client.send(json!({"type":"control_response","response":{"subtype":"success","request_id":request["request_id"],"response":output}})).await;
}

async fn initialize(client: &mut Client, hooks: Value) {
    client
        .send(initialize_request("hooks", json!({"hooks":hooks})))
        .await;
    let response = client.frame("control_response").await;
    assert_eq!(response["response"]["subtype"], "success", "{response}");
}

#[tokio::test]
async fn sdk_callbacks_run_before_first_turn_and_rewrite_actual_tool_input() {
    for anthropic in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let server = Server::start(anthropic, Mode::Approval).await;
        let data = root.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("settings.json"), json!({"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"echo COMMAND_START_CONTEXT"}]}]}}).to_string()).unwrap();
        let mut client = Client::start(root.path(), &server, &["--permission-mode", "ask"]);
        initialize(
            &mut client,
            json!({
                "SessionStart":[{"matcher":"^startup$","hookCallbackIds":["start"]}],
                "PreToolUse":[{"matcher":"^Write$","hookCallbackIds":["pre"]}],
                "PostToolUse":[{"matcher":"^Write$","hookCallbackIds":["post"]}]
            }),
        )
        .await;
        let startup = client.frame("control_request").await;
        assert_eq!(startup["request"]["callback_id"], "start");
        assert_eq!(startup["request"]["input"]["source"], "startup");
        client.send(user("Write the fixture")).await;
        assert!(server.fixture.requests.lock().unwrap().is_empty());
        answer(&mut client, &startup, json!({"hookSpecificOutput":{"hookEventName":"SessionStart","additionalContext":"SDK_START_CONTEXT"}})).await;
        let pre = client.frame("control_request").await;
        assert_eq!(pre["request"]["callback_id"], "pre");
        assert_eq!(pre["request"]["tool_use_id"], "write-1");
        assert_eq!(
            pre["request"]["input"]["tool_input"]["file_path"],
            "approval.txt"
        );
        answer(&mut client, &pre, json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow","updatedInput":{"file_path":"callback.txt","content":"CALLBACK_WRITE"}}})).await;
        let post = client.frame("control_request").await;
        assert_eq!(post["request"]["callback_id"], "post");
        assert_eq!(post["request"]["tool_use_id"], "write-1");
        assert_eq!(
            post["request"]["input"]["tool_input"]["file_path"],
            "callback.txt"
        );
        answer(&mut client, &post, json!({})).await;
        assert_eq!(client.result().await["is_error"], false);
        assert_eq!(
            std::fs::read_to_string(root.path().join("callback.txt")).unwrap(),
            "CALLBACK_WRITE"
        );
        assert!(!root.path().join("approval.txt").exists());
        let first = server.fixture.requests.lock().unwrap()[0].to_string();
        assert!(first.contains("SDK_START_CONTEXT"), "{first}");
        assert!(first.contains("COMMAND_START_CONTEXT"), "{first}");
        assert!(client.finish().await.0.success());
        // Registration is run-local: a new process must not ask for prior callbacks.
        let fresh = Server::start(anthropic, Mode::Approval).await;
        let mut client = Client::start(root.path(), &fresh, &[]);
        client.send(user("Write without SDK callbacks")).await;
        assert_eq!(client.result().await["is_error"], false);
        assert!(root.path().join("approval.txt").exists());
        assert!(client.finish().await.0.success());
    }
}

#[tokio::test]
async fn sdk_permission_callback_races_host_and_discards_late_losing_policy() {
    for anthropic in [false, true] {
        for hook_wins in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let server = Server::start(anthropic, Mode::Approval).await;
            let mut client = Client::start(root.path(), &server, &["--permission-mode", "ask"]);
            initialize(
                &mut client,
                json!({"PermissionRequest":[{"matcher":"Write","hookCallbackIds":["permission"]}]}),
            )
            .await;
            client.send(user("Write with competing approvals")).await;
            let first = client.frame("control_request").await;
            let second = client.frame("control_request").await;
            let (hook, host) = if first["request"]["subtype"] == "hook_callback" {
                (first, second)
            } else {
                (second, first)
            };
            assert_eq!(hook["request"]["callback_id"], "permission");
            assert_eq!(host["request"]["subtype"], "can_use_tool");
            let hook_output = json!({"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow","updatedInput":{"file_path":"callback.txt","content":"CALLBACK"},"updatedPermissions":[{"type":"addRules","destination":"session","behavior":"allow","rules":[{"toolName":"Write"}]}]}}});
            if hook_wins {
                answer(&mut client, &hook, hook_output.clone()).await;
            } else {
                answer(&mut client, &host, json!({"behavior":"allow","updatedInput":{"file_path":"host.txt","content":"HOST"}})).await;
            }
            let cancelled = client.frame("control_cancel_request").await;
            assert_eq!(
                &cancelled["request_id"],
                if hook_wins {
                    &host["request_id"]
                } else {
                    &hook["request_id"]
                }
            );
            if hook_wins {
                answer(&mut client, &host, json!({"behavior":"allow","updatedInput":{"file_path":"late.txt","content":"LATE"},"updatedPermissions":[{"type":"addRules","destination":"session","behavior":"deny","rules":[{"toolName":"Write"}]}]})).await;
            } else {
                answer(&mut client, &hook, hook_output).await;
            }
            assert_eq!(client.result().await["is_error"], false);
            assert_eq!(root.path().join("callback.txt").exists(), hook_wins);
            assert_eq!(root.path().join("host.txt").exists(), !hook_wins);
            assert!(!root.path().join("late.txt").exists());
            client
                .send(user("Write again to verify only the winner's policy"))
                .await;
            if !hook_wins {
                let first = client.frame("control_request").await;
                let second = client.frame("control_request").await;
                let (hook, host) = if first["request"]["subtype"] == "hook_callback" {
                    (first, second)
                } else {
                    (second, first)
                };
                answer(&mut client, &hook, json!({})).await;
                answer(&mut client, &host, json!({"behavior":"deny"})).await;
            }
            assert_eq!(client.result().await["is_error"], false);
            assert_eq!(root.path().join("approval.txt").exists(), hook_wins);
            assert!(client.finish().await.0.success());
        }
    }
}

#[tokio::test]
async fn sdk_callback_timeout_and_wrong_response_type_never_approve_host_request() {
    for anthropic in [false, true] {
        for timeout in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let server = Server::start(anthropic, Mode::Approval).await;
            let mut client = Client::start(root.path(), &server, &["--permission-mode", "ask"]);
            initialize(&mut client, json!({"PreToolUse":[{"matcher":"Write","hookCallbackIds":["pre"],"timeout":if timeout {0.1} else {60.0}}]})).await;
            client
                .send(user("Write only after actual host approval"))
                .await;
            let hook = client.frame("control_request").await;
            assert_eq!(hook["request"]["subtype"], "hook_callback");
            if timeout {
                let cancel = client.frame("control_cancel_request").await;
                assert_eq!(cancel["request_id"], hook["request_id"]);
                answer(&mut client, &hook, json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow"}})).await;
            } else {
                // A can_use_tool-shaped response on a callback ID is not a permission grant.
                answer(&mut client, &hook, json!({"behavior":"allow","updatedPermissions":[{"type":"addRules","destination":"session","behavior":"allow","rules":[{"toolName":"Write"}]}]})).await;
            }
            let host = client.frame("control_request").await;
            assert_eq!(host["request"]["subtype"], "can_use_tool");
            assert!(!root.path().join("approval.txt").exists());
            answer(&mut client, &host, json!({"behavior":"deny"})).await;
            assert_eq!(client.result().await["is_error"], false);
            assert!(!root.path().join("approval.txt").exists());
            assert!(client.finish().await.0.success());
        }
    }
}

#[tokio::test]
async fn sdk_callback_startup_eof_interrupt_and_unknown_ids_terminate_without_model_requests() {
    for mode in ["eof", "eof-queued", "interrupt", "unknown", "duplicate"] {
        let root = tempfile::tempdir().unwrap();
        let server = Server::start(false, Mode::Text).await;
        let mut client = Client::start(root.path(), &server, &[]);
        initialize(
            &mut client,
            json!({"SessionStart":[{"hookCallbackIds":["start"]}]}),
        )
        .await;
        let startup = client.frame("control_request").await;
        if mode == "interrupt" {
            client.send(json!({"type":"control_request","request_id":"interrupt","request":{"subtype":"interrupt"}})).await;
            client.frame("control_response").await;
        } else if mode == "unknown" {
            answer(
                &mut client,
                &json!({"request_id":"never-issued"}),
                json!({}),
            )
            .await;
        }
        if mode == "eof-queued" {
            client
                .send(user("Must never reach a model after startup disconnect"))
                .await;
        }
        if mode == "duplicate" {
            answer(&mut client, &startup, json!({})).await;
            answer(&mut client, &startup, json!({})).await;
        }
        let (status, results) = client.finish().await;
        assert_eq!(status.success(), mode == "eof");
        assert!(results.len() <= 1);
        assert!(server.fixture.requests.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn sdk_interrupt_cancels_callback_once_and_preserves_registration_for_later_turns() {
    for anthropic in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let server = Server::start(anthropic, Mode::Approval).await;
        let mut client = Client::start(root.path(), &server, &[]);
        initialize(
            &mut client,
            json!({"PreToolUse":[{"matcher":"Write","hookCallbackIds":["pre"]}]}),
        )
        .await;
        client.send(user("Interrupt this write")).await;
        let callback = client.frame("control_request").await;
        client.send(json!({"type":"control_request","request_id":"stop","request":{"subtype":"interrupt"}})).await;
        assert_eq!(
            client.frame("control_cancel_request").await["request_id"],
            callback["request_id"]
        );
        assert_eq!(
            client.frame("control_response").await["response"]["request_id"],
            "stop"
        );
        assert_eq!(client.result().await["is_error"], true);
        answer(&mut client, &callback, json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow"}})).await;
        assert!(!root.path().join("approval.txt").exists());
        client.send(user("Continue with text")).await;
        assert_eq!(client.result().await["is_error"], false);
        client
            .send(user("Now write with the registered callback"))
            .await;
        let callback = client.frame("control_request").await;
        assert_eq!(callback["request"]["callback_id"], "pre");
        answer(&mut client, &callback, json!({})).await;
        assert_eq!(client.result().await["is_error"], false);
        assert!(root.path().join("approval.txt").exists());
        assert!(!client.finish().await.0.success());
    }
}

#[tokio::test]
async fn sdk_callback_registration_is_inherited_by_owned_child_with_child_identity() {
    for anthropic in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let server = Server::start(anthropic, Mode::RuntimeAgent).await;
        let mut client = Client::start(root.path(), &server, &[]);
        client.send(initialize_request("child", json!({
            "agents":{"sdk-child":{"description":"Callback child","prompt":"CHILD_CALLBACK_PROMPT","tools":["Read","Write"],"model":"inherit","maxTurns":3}},
            "hooks":{"PreToolUse":[{"matcher":"^(Agent|Write)$","hookCallbackIds":["pre"]}]}
        }))).await;
        assert_eq!(
            client.frame("control_response").await["response"]["subtype"],
            "success"
        );
        client.send(user("Delegate the task to sdk-child")).await;
        let parent = client.frame("control_request").await;
        assert_eq!(parent["request"]["input"]["tool_name"], "Agent");
        answer(&mut client, &parent, json!({})).await;
        let child = client.frame("control_request").await;
        assert_eq!(child["request"]["input"]["tool_name"], "Write");
        assert_ne!(
            child["request"]["input"]["session_id"],
            parent["request"]["input"]["session_id"]
        );
        assert_eq!(child["request"]["tool_use_id"], "write-1");
        answer(&mut client, &child, json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","updatedInput":{"file_path":"child-callback.txt","content":"CHILD_CALLBACK"}}})).await;
        assert_eq!(client.result().await["is_error"], false);
        assert!(root.path().join("child-callback.txt").exists());
        assert!(!root.path().join("child.txt").exists());
        assert!(client.finish().await.0.success());
    }
}

#[tokio::test]
async fn sdk_eof_during_model_or_callback_cannot_continue_automatically_allowed_write() {
    for anthropic in [false, true] {
        for before_callback in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let server = Server::start(
                anthropic,
                if before_callback {
                    Mode::DelayedApproval
                } else {
                    Mode::Approval
                },
            )
            .await;
            let mut client = Client::start(root.path(), &server, &[]);
            initialize(
                &mut client,
                json!({"PreToolUse":[{"matcher":"Write","hookCallbackIds":["pre"]}]}),
            )
            .await;
            client
                .send(user("Write with accept-edits mode but live SDK hooks"))
                .await;
            if before_callback {
                tokio::time::timeout(Duration::from_secs(10), async {
                    while server.fixture.requests.lock().unwrap().is_empty() {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await
                .unwrap();
            } else {
                let callback = client.frame("control_request").await;
                assert_eq!(callback["request"]["callback_id"], "pre");
                client
                    .send(user(
                        "Queued work must be discarded when callback host disconnects",
                    ))
                    .await;
            }
            let (status, results) = client.finish().await;
            assert!(!status.success());
            assert_eq!(results.len(), 1, "{results:?}");
            assert_eq!(results[0]["is_error"], true);
            assert!(!root.path().join("approval.txt").exists());
            assert_eq!(server.fixture.requests.lock().unwrap().len(), 1);
        }
    }
}

#[tokio::test]
async fn initial_message_precedes_queued_user_and_is_not_authority() {
    for queued in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let server = Server::start(false, Mode::Text).await;
        let mut client = Client::start(root.path(), &server, &["--replay-user-messages"]);
        initialize(&mut client, json!({"SessionStart":[{"hookCallbackIds":["start"]}],"UserPromptSubmit":[{"hookCallbackIds":["submit"]}]})).await;
        let start = client.frame("control_request").await;
        if queued {
            client.send(user("HUMAN_INPUT")).await;
        }
        answer(&mut client, &start, json!({"hookSpecificOutput":{"hookEventName":"SessionStart","initialUserMessage":"HOOK_INITIAL"}})).await;
        let replay = client.frame("user").await;
        assert_eq!(replay["origin"]["hookEvent"], "SessionStart");
        let submit = client.frame("control_request").await;
        assert_eq!(submit["request"]["input"]["prompt"], "HOOK_INITIAL");
        answer(&mut client, &submit, json!({})).await;
        let result = client.result().await;
        assert_eq!(result["is_error"], false);
        let id = result["session_id"].as_str().unwrap();
        if queued {
            let submit = client.frame("control_request").await;
            assert_eq!(submit["request"]["input"]["prompt"], "HUMAN_INPUT");
            answer(&mut client, &submit, json!({})).await;
            assert_eq!(client.result().await["is_error"], false);
        }
        let (status, extra) = client.finish().await;
        assert!(status.success());
        assert!(extra.is_empty());
        let requests = server.fixture.requests.lock().unwrap();
        assert_eq!(requests.len(), if queued { 2 } else { 1 });
        assert!(requests[0].to_string().contains("HOOK_INITIAL"));
        assert!(!requests[0].to_string().contains("HUMAN_INPUT"));
        let context =
            deepseek_code_core::services::session_messages::context(&root.path().join("data")).unwrap();
        let authority =
            deepseek_code_core::services::permission_authority::history(&context.db, id, 8192).unwrap();
        assert!(!serde_json::to_string(&authority)
            .unwrap()
            .contains("HOOK_INITIAL"));
        assert_eq!(authority.len(), usize::from(queued));
    }
}
#[tokio::test]
async fn timed_out_initial_message_cannot_reappear_on_later_user_turn() {
    let root = tempfile::tempdir().unwrap();
    let server = Server::start(false, Mode::Text).await;
    let mut client = Client::start(root.path(), &server, &[]);
    initialize(
        &mut client,
        json!({"SessionStart":[{"hookCallbackIds":["start"],"timeout":0.1}]}),
    )
    .await;
    let start = client.frame("control_request").await;
    let cancel = client.frame("control_cancel_request").await;
    assert_eq!(cancel["request_id"], start["request_id"]);
    answer(&mut client, &start, json!({"hookSpecificOutput":{"hookEventName":"SessionStart","initialUserMessage":"LATE_INITIAL"}})).await;
    client.send(user("HUMAN_AFTER_TIMEOUT")).await;
    assert_eq!(client.result().await["is_error"], false);
    let (status, extra) = client.finish().await;
    assert!(status.success());
    assert!(extra.is_empty());
    let requests = server.fixture.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(!requests[0].to_string().contains("LATE_INITIAL"));
}
