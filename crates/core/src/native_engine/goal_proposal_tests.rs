use super::*;

#[tokio::test]
async fn goal_proposals_are_local_main_session_only_and_return_without_waiting() {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let root = tempfile::tempdir().unwrap();
    let db = Arc::new(crate::db::DbManager::new(root.path().join("state.db")).unwrap());
    db.init().unwrap();
    let session = crate::runtime::SessionStore::new(db.clone())
        .create(root.path(), "fixture", "fixture")
        .unwrap();
    let permissions = Arc::new(crate::permissions::PermissionManager::new(Arc::new(
        crate::permissions::AuditLogger::new(128),
    )));
    let (events, _receive) = mpsc::channel(64);
    let provider = ResolvedProvider {
        provider: serde_json::from_value(json!({"id":"fixture","name":"fixture","base_url":"http://127.0.0.1:1","api_key":"unused","api_format":"openai","models":[],"enabled":true})).unwrap(),
        model: serde_json::from_value(json!({"id":"fixture","name":"fixture","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap(),
    };
    let mut executor = ToolLoopExecutor::new(
        provider,
        vec![],
        None,
        128,
        events,
        root.path().to_string_lossy().into_owned(),
        ReasoningMode::Standard,
    )
    .with_goal_state(db.clone())
    .with_conv_id(session.clone())
    .with_search_config_path(root.path().join("config.json"))
    .with_permission_manager(permissions.clone());
    // This test exercises frontend availability, independently of discovery.
    std::fs::write(
        root.path().join("customization.json"),
        json!({"tool_loading":{"ProposeGoal":{"enabled":true,"on_demand":false}}}).to_string(),
    )
    .unwrap();
    let input = json!({"condition":"Run the tests"});
    assert!(!executor
        .tool_definitions()
        .unwrap()
        .iter()
        .any(|tool| tool.name == "ProposeGoal"));
    assert!(
        executor
            .execute_tool_call("ProposeGoal", &input, "headless")
            .await
            .2
    );
    let frontend =
        crate::services::goal_proposals::Frontend::acquire(db.clone(), &session).unwrap();
    // Another engine sharing the database remains headless unless its host
    // connects it to the actual local terminal worker.
    assert!(!executor
        .tool_definitions()
        .unwrap()
        .iter()
        .any(|tool| tool.name == "ProposeGoal"));
    executor.local_goal_frontend = true;
    assert!(executor
        .tool_definitions()
        .unwrap()
        .iter()
        .any(|tool| tool.name == "ProposeGoal"));
    let (_, output, failed) = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        executor.execute_tool_call("ProposeGoal", &input, "main"),
    )
    .await
    .unwrap();
    assert!(!failed, "{output}");
    let proposal = frontend.poll().unwrap().unwrap();
    assert!(!proposal.approved);
    assert!(
        crate::services::permission_authority::history(&db, &session, 8192)
            .unwrap()
            .is_empty()
    );
    frontend
        .decide(&proposal.id, false, root.path(), root.path(), &permissions)
        .unwrap();
    executor.delegation_depth = 1;
    assert!(!executor
        .tool_definitions()
        .unwrap()
        .iter()
        .any(|tool| tool.name == "ProposeGoal"));
    assert!(
        executor
            .execute_tool_call("ProposeGoal", &json!({"condition":"Child target"}), "child")
            .await
            .2
    );
    executor.delegation_depth = 0;
    permissions.set_mode(crate::permissions::PermissionMode::PlanMode);
    assert!(!executor
        .tool_definitions()
        .unwrap()
        .iter()
        .any(|tool| tool.name == "ProposeGoal"));
    assert!(
        executor
            .execute_tool_call("ProposeGoal", &json!({"condition":"Plan target"}), "plan")
            .await
            .2
    );
    permissions.set_mode(crate::permissions::PermissionMode::BypassPermissions);
    permissions.set_session_rules(
        &session,
        std::collections::HashMap::from([(
            "ProposeGoal".into(),
            crate::permissions::rules::PermissionAction::Deny,
        )]),
    );
    assert!(!executor
        .tool_definitions()
        .unwrap()
        .iter()
        .any(|tool| tool.name == "ProposeGoal"));
    assert!(
        executor
            .execute_tool_call(
                "ProposeGoal",
                &json!({"condition":"Denied target"}),
                "denied"
            )
            .await
            .2
    );
}
