use super::*;
use crate::{
    runtime::{RuntimePaths, SessionStore},
    services::{EnvironmentCredentials, ServiceContext},
};
use axum::{routing::post, Json, Router};

pub(super) fn fixture() -> (
    tempfile::TempDir,
    ServiceContext,
    SessionStore,
    ToolLoopExecutor,
    mpsc::Receiver<EngineEvent>,
    Arc<crate::permissions::PermissionManager>,
) {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let context = ServiceContext::open(
        RuntimePaths::new(temp.path().join("data")).unwrap(),
        Arc::new(EnvironmentCredentials),
    )
    .unwrap();
    let sessions = SessionStore::new(context.db.clone());
    let id = sessions.create(temp.path(), "fixture", "fixture").unwrap();
    sessions
        .append(&id, "user", "OLD_HISTORY_SENTINEL")
        .unwrap();
    let manager = crate::runtime::default_permissions();
    manager.set_mode(PermissionMode::PlanMode);
    let provider=ResolvedProvider{provider:serde_json::from_value(json!({"id":"fixture","name":"fixture","base_url":"http://127.0.0.1:1","api_key":"placeholder","api_format":"openai","models":[],"enabled":true})).unwrap(),model:serde_json::from_value(json!({"id":"fixture","name":"fixture","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap()};
    let (tx, rx) = mpsc::channel(200);
    let executor = ToolLoopExecutor::new(
        provider,
        vec![json!({"role":"user","content":"OLD_HISTORY_SENTINEL"})],
        Some("CURRENT_SYSTEM".into()),
        1024,
        tx,
        temp.path().to_string_lossy().into(),
        ReasoningMode::Standard,
    )
    .with_conv_id(id)
    .with_search_config_path(context.paths.data_dir.join("config.json"))
    .with_permission_manager(manager.clone());
    (temp, context, sessions, executor, rx, manager)
}

#[tokio::test]
async fn bypass_plan_requires_origin_and_explicit_approval_and_survives_worker_handoff() {
    // Full Access is process-wide; isolate this test from parallel permission tests.
    if std::env::var_os("DCL_BYPASS_PLAN_TEST_CHILD").is_none() {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "native_engine::tool_loop::plan_tool::approval_tests::bypass_plan_requires_origin_and_explicit_approval_and_survives_worker_handoff", "--test-threads=1"])
            .env("DCL_BYPASS_PLAN_TEST_CHILD", "1")
            .status().unwrap();
        assert!(status.success());
        return;
    }
    use crate::services::session_permission_mode as modes;
    for answer in [
        "bypass_keep",
        "ordinary",
        "1",
        "3",
        "4",
        "cancel",
        "changed",
        "revoked",
        "db_failure",
    ] {
        let (_temp, context, _sessions, mut executor, mut events, manager) = fixture();
        let session = executor.conv_id.clone().unwrap();
        manager.set_plan_launch_permissions(true, answer != "ordinary");
        modes::set(
            &context.db,
            &session,
            &manager,
            PermissionMode::BypassPermissions,
        )
        .unwrap();
        executor
            .plan_transition("EnterPlanMode", &json!({}), "enter")
            .await
            .unwrap();
        assert!(modes::plan_returns_to_bypass(&context.db, &session, &manager).unwrap());
        // A newly started worker restores the origin, but needs its own launch authority.
        let worker = crate::runtime::default_permissions();
        worker.set_plan_launch_permissions(true, true);
        modes::set(&context.db, &session, &worker, PermissionMode::PlanMode).unwrap();
        assert!(modes::plan_returns_to_bypass(&context.db, &session, &worker).unwrap());
        worker.set_plan_launch_permissions(false, false);
        assert!(!modes::plan_returns_to_bypass(&context.db, &session, &worker).unwrap());
        if answer == "db_failure" {
            context.db.with_conn(|conn| conn.execute_batch("CREATE TRIGGER fail_bypass BEFORE UPDATE ON peripheral_settings WHEN NEW.value='bypass' BEGIN SELECT RAISE(ABORT,'fixture failure'); END;")).unwrap().unwrap();
        }
        let waiters = executor.answer_waiters.clone();
        let input = json!({"plan":"# Return to bypass after approval"});
        let mut future = Box::pin(executor.plan_transition("ExitPlanMode", &input, "exit"));
        let event = tokio::select! { event = events.recv() => event.unwrap(), result = &mut future => panic!("No approval: {result:?}") };
        let EngineEvent::AskUser {
            request_id,
            tool_input,
            options,
            ..
        } = event
        else {
            panic!("Wrong event")
        };
        assert_eq!(tool_input["returnToBypass"], true);
        assert!(options[0].contains("bypass") && options[1].contains("bypass"));
        assert_eq!(manager.get_mode(), PermissionMode::PlanMode);
        if answer == "cancel" {
            drop(future);
            assert!(waiters.lock().await.is_empty());
            continue;
        }
        if answer == "changed" {
            manager.set_mode(PermissionMode::AskPermissions);
            manager.set_mode(PermissionMode::PlanMode);
        }
        if answer == "revoked" {
            manager.set_plan_launch_permissions(false, false);
        }
        waiters
            .lock()
            .await
            .remove(&request_id)
            .unwrap()
            .send(
                if matches!(answer, "changed" | "revoked" | "db_failure" | "ordinary") {
                    "bypass_keep"
                } else {
                    answer
                }
                .into(),
            )
            .unwrap();
        let result = future.await;
        assert_eq!(crate::permissions::full_access(), matches!(answer, "bypass_keep" | "1"));
        crate::permissions::set_full_access(false);
        assert_eq!(
            result.is_err(),
            matches!(answer, "changed" | "revoked" | "db_failure"),
            "{answer}: {result:?}"
        );
        let expected = match answer {
            "bypass_keep" | "ordinary" | "1" => PermissionMode::BypassPermissions,
            "3" => PermissionMode::AskPermissions,
            _ => PermissionMode::PlanMode,
        };
        assert_eq!(manager.get_mode(), expected, "{answer}");
        assert_eq!(modes::load(&context.db, &session).unwrap(), Some(expected));
        assert_eq!(
            crate::services::plan_approval::latest_boundary(&context.db, &session)
                .unwrap()
                .is_some(),
            answer == "1"
        );
        if let Ok(result) = result {
            assert_eq!(result["mode"], modes::name(expected));
        }
    }
}

#[tokio::test]
async fn approval_choices_preserve_rules_cancel_waiters_and_rollback_failed_boundary() {
    for choice in [
        "1",
        "2",
        "3",
        "4",
        "feedback: add tests",
        "auto",
        "bypassPermissions",
        "bypass_keep",
        "db_failure",
        "cancel",
    ] {
        let (_temp, context, _sessions, mut executor, mut events, manager) = fixture();
        let session = executor.conv_id.clone().unwrap();
        manager.set_session_rules(
            &session,
            std::collections::HashMap::from([(
                "Write".into(),
                crate::permissions::rules::PermissionAction::Deny,
            )]),
        );
        if choice == "db_failure" {
            context.db.with_conn(|c|c.execute_batch("CREATE TRIGGER fail_boundary BEFORE INSERT ON messages WHEN NEW.is_compact_boundary=1 BEGIN SELECT RAISE(ABORT,'fixture write failed'); END;")).unwrap().unwrap();
        }
        let waiters = executor.answer_waiters.clone();
        let input = json!({"plan":"# approved fixture plan"});
        let mut future = Box::pin(executor.plan_transition("ExitPlanMode", &input, "exit"));
        let event = tokio::select! {event=events.recv()=>event.unwrap(), result=&mut future=>panic!("No approval: {result:?}")};
        let EngineEvent::AskUser { request_id, .. } = event else {
            panic!("Wrong event")
        };
        if choice == "cancel" {
            drop(future);
            assert!(waiters.lock().await.is_empty());
            assert_eq!(manager.get_mode(), PermissionMode::PlanMode);
            continue;
        }
        waiters
            .lock()
            .await
            .remove(&request_id)
            .unwrap()
            .send(if choice == "db_failure" { "1" } else { choice }.into())
            .unwrap();
        let result = future.await;
        assert_eq!(
            result.is_err(),
            matches!(
                choice,
                "auto" | "bypassPermissions" | "bypass_keep" | "db_failure"
            ),
            "{choice}: {result:?}"
        );
        assert_eq!(
            manager.get_mode(),
            match choice {
                "1" | "2" => PermissionMode::AcceptEdits,
                "3" => PermissionMode::AskPermissions,
                _ => PermissionMode::PlanMode,
            }
        );
        assert_eq!(
            crate::services::plan_approval::latest_boundary(&context.db, &session)
                .unwrap()
                .is_some(),
            choice == "1"
        );
        assert!(matches!(
            executor
                .check_permission(
                    "Write",
                    &json!({"file_path":executor.workspace_cwd.clone()+"/denied","content":"bad"}),
                    "denied-after-plan"
                )
                .await,
            PermissionResult::Denied(_)
        ));
        assert!(waiters.lock().await.is_empty());
    }
}

#[tokio::test]
async fn clear_plan_context_skips_stale_tools_and_survives_resume_on_both_protocols() {
    for format in ["openai", "anthropic"] {
        let (temp, context, sessions, mut executor, mut events, manager) = fixture();
        let target = temp.path().join("must-not-exist.txt");
        let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
        let recorder = requests.clone();
        let output = target.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        executor.provider.provider.base_url = format!("http://{}", listener.local_addr().unwrap());
        executor.provider.provider.api_format = if format == "openai" {
            crate::native_engine::provider_manager::ApiFormat::OpenAI
        } else {
            crate::native_engine::provider_manager::ApiFormat::Anthropic
        };
        let app=Router::new().route(if format=="openai"{"/v1/chat/completions"}else{"/v1/messages"},post(move |Json(body):Json<Value>|{let recorder=recorder.clone();let target=output.clone();async move {
            let mut requests=recorder.lock().await;let first=requests.is_empty();requests.push(body);
            let calls=[("exit","ExitPlanMode",json!({"plan":"# APPROVED_PLAN\nImplement and test"})),("stale","Write",json!({"file_path":target,"content":"forbidden stale batch"}))];
            let events=if format=="openai" {if first {vec![json!({"choices":[{"index":0,"delta":{"tool_calls":calls.iter().enumerate().map(|(index,(id,name,args))|json!({"index":index,"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}})).collect::<Vec<_>>()},"finish_reason":"tool_calls"}]})]}else{vec![json!({"choices":[{"index":0,"delta":{"content":"IMPLEMENTED"},"finish_reason":"stop"}]})]}}else{
                let mut events=vec![json!({"type":"message_start","message":{"id":"m","model":"fixture","usage":{"input_tokens":1}}})];
                if first {for (index,(id,name,input)) in calls.into_iter().enumerate(){events.extend([json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":id,"name":name,"input":{}}}),json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":input.to_string()}}),json!({"type":"content_block_stop","index":index})]);}}else{events.extend([json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"IMPLEMENTED"}}),json!({"type":"content_block_stop","index":0})]);}
                events.extend([json!({"type":"message_delta","delta":{"stop_reason":if first{"tool_use"}else{"end_turn"}},"usage":{"output_tokens":1}}),json!({"type":"message_stop"})]);events
            };let mut stream=events.into_iter().map(|v|format!("data: {v}\n\n")).collect::<String>();if format=="openai"{stream.push_str("data: [DONE]\n\n");} ([("content-type","text/event-stream")],stream)
        }}));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let waiters = executor.answer_waiters.clone();
        let reader = tokio::spawn(async move {
            let mut skipped = false;
            let mut reset = false;
            while let Some(event) = events.recv().await {
                match event {
                    EngineEvent::AskUser { request_id, .. } => {
                        waiters
                            .lock()
                            .await
                            .remove(&request_id)
                            .unwrap()
                            .send("1".into())
                            .unwrap();
                    }
                    EngineEvent::ToolUseDone {
                        tool_use_id,
                        is_error,
                        output,
                        ..
                    } if tool_use_id == "stale" => {
                        skipped = is_error && output.contains("skipped");
                    }
                    EngineEvent::ContextReset { .. } => reset = true,
                    _ => {}
                }
            }
            (skipped, reset)
        });
        let result = tokio::time::timeout(Duration::from_secs(20), executor.execute())
            .await
            .unwrap()
            .unwrap();
        let session = executor.conv_id.clone().unwrap();
        drop(executor);
        assert_eq!(reader.await.unwrap(), (true, true));
        server.abort();
        assert_eq!(result.0, "IMPLEMENTED");
        assert!(!target.exists());
        assert_eq!(manager.get_mode(), PermissionMode::AcceptEdits);
        let requests = requests.lock().await;
        assert_eq!(requests.len(), 2);
        assert!(!requests[1]["messages"]
            .to_string()
            .contains("OLD_HISTORY_SENTINEL"));
        assert!(requests[1].to_string().contains("APPROVED_PLAN"));
        assert!(requests[1].to_string().contains("CURRENT_SYSTEM"));
        let restored = sessions
            .model_history(
                &session,
                &crate::native_engine::provider_manager::ApiFormat::OpenAI,
            )
            .unwrap();
        assert!(!serde_json::to_string(&restored)
            .unwrap()
            .contains("OLD_HISTORY_SENTINEL"));
        assert!(serde_json::to_string(&restored)
            .unwrap()
            .contains("APPROVED_PLAN"));
        assert!(
            crate::services::plan_approval::latest_boundary(&context.db, &session)
                .unwrap()
                .is_some()
        );
    }
}

#[tokio::test]
async fn teammate_plan_approval_routes_to_leader_and_never_self_approves() {
    use crate::services::{session_messages as live, teams};
    std::env::set_var("CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS", "1");
    let (_temp, context, sessions, mut executor, mut events, manager) = fixture();
    let member_session = executor.conv_id.clone().unwrap();
    let lead = sessions
        .create(
            std::path::Path::new(&executor.workspace_cwd),
            "fixture",
            "fixture",
        )
        .unwrap();
    let team = teams::create(
        &context.paths.data_dir,
        &lead,
        "approval",
        None,
        None,
        None,
        std::path::Path::new(&executor.workspace_cwd),
    )
    .unwrap()
    .team;
    let member = teams::register_member(
        &context.paths.data_dir,
        &team.name,
        &lead,
        "alpha",
        None,
        None,
        "plan",
        std::path::Path::new(&executor.workspace_cwd),
    )
    .unwrap();
    teams::bind_member_session(
        &context.paths.data_dir,
        &team.name,
        &member.agent_id,
        &member_session,
    )
    .unwrap();

    let input = json!({"plan":"# teammate plan\n\nDo the work."});
    let submitted = executor
        .plan_transition("ExitPlanMode", &input, "exit")
        .await
        .unwrap();
    assert_eq!(submitted["submitted"], true);
    assert_eq!(submitted["approved"], false);
    assert_eq!(manager.get_mode(), PermissionMode::PlanMode);
    assert!(
        events.try_recv().is_err(),
        "teammate must not prompt the user"
    );
    let requests_file = context
        .paths
        .data_dir
        .join("teams")
        .join(&team.name)
        .join("requests.json");
    let request = teams::pending_plan_request(&context.paths.data_dir, &team.name, &member_session)
        .unwrap()
        .unwrap_or_else(|| {
            panic!(
                "request (member {member_session}, team {}): {}",
                team.name,
                std::fs::read_to_string(requests_file).unwrap_or_default()
            )
        });
    assert!(live::inbox(&context, &lead, false)
        .unwrap()
        .iter()
        .any(|message| message.content.contains("plan_approval_request")));

    let rejected = crate::tools::agent_messages::execute_live(
        &context,
        &lead,
        json!({"operation":"plan_approval_response","request_id":request.id,"approve":false,"feedback":"add tests"}),
    )
    .unwrap();
    assert_eq!(rejected["state"], "rejected");
    let denied = executor
        .plan_transition("ExitPlanMode", &input, "exit")
        .await
        .unwrap();
    assert_eq!(denied["approved"], false);
    assert_eq!(denied["feedback"], "add tests");
    assert_eq!(manager.get_mode(), PermissionMode::PlanMode);

    // Request again and approve; the teammate then leaves plan mode on retry.
    let again = executor
        .plan_transition("ExitPlanMode", &input, "exit")
        .await
        .unwrap();
    assert_eq!(again["submitted"], true);
    let request = teams::pending_plan_request(&context.paths.data_dir, &team.name, &member_session)
        .unwrap()
        .expect("second request");
    crate::tools::agent_messages::execute_live(
        &context,
        &lead,
        json!({"operation":"plan_approval_response","request_id":request.id,"approve":true}),
    )
    .unwrap();
    let approved = executor
        .plan_transition("ExitPlanMode", &input, "exit")
        .await
        .unwrap();
    assert_eq!(approved["approved"], true);
    assert_eq!(approved["mode"], "accept_edits");
    assert_eq!(manager.get_mode(), PermissionMode::AcceptEdits);
}

#[tokio::test]
async fn allowed_prompts_are_recorded_but_never_grant_bash() {
    let (temp, _context, _sessions, mut executor, mut events, manager) = fixture();
    let input = json!({"plan":"# plan\n\nrun tests","allowedPrompts":[{"tool":"Bash","prompt":"run tests"}]});
    let waiters = executor.answer_waiters.clone();
    let mut future = Box::pin(executor.plan_transition("ExitPlanMode", &input, "exit"));
    let event = tokio::select! {event=events.recv()=>event.unwrap(), result=&mut future=>panic!("No approval: {result:?}")};
    let EngineEvent::AskUser { request_id, .. } = event else {
        panic!("Wrong event")
    };
    waiters
        .lock()
        .await
        .remove(&request_id)
        .unwrap()
        .send("2".into())
        .unwrap();
    let approved = future.await.unwrap();
    assert_eq!(approved["approved"], true);
    assert_eq!(approved["allowedPrompts"][0]["prompt"], "run tests");
    // Semantic prompts do not become blanket Bash grants.
    assert!(matches!(
        executor
            .check_permission("Bash", &json!({"command":"cargo test"}), "bash-after-plan")
            .await,
        PermissionResult::RequiresConfirmation(_)
    ));
    drop(temp);
    manager.set_mode(PermissionMode::PlanMode);
    assert!(executor
        .plan_transition(
            "ExitPlanMode",
            &json!({"plan":"# p","allowedPrompts":[{"tool":"Write","prompt":"x"}]}),
            "bad"
        )
        .await
        .is_err());
    assert_eq!(manager.get_mode(), PermissionMode::PlanMode);
}
