use super::*;
use crate::native_engine::{engine_core::ReasoningMode, provider_manager::ResolvedProvider};
use crate::permissions::rules::PermissionAction;

#[tokio::test]
async fn resource_reads_keep_explicit_permissions_and_model_api_invocation() {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
    let provider = ResolvedProvider {
        provider: serde_json::from_value(json!({"id":"fixture","name":"fixture","base_url":"http://127.0.0.1:1","api_key":"unused","api_format":"openai","models":[],"enabled":true})).unwrap(),
        model: serde_json::from_value(json!({"id":"fixture","name":"fixture","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap(),
    };
    let (tx, _rx) = tokio::sync::mpsc::channel(32);
    let manager = crate::runtime::default_permissions();
    manager.set_mode(crate::permissions::PermissionMode::BypassPermissions);
    manager.set_session_rules(
        "bundle-fixture",
        std::collections::HashMap::from([("Read".into(), PermissionAction::Deny)]),
    );
    let mut executor = ToolLoopExecutor::new(
        provider,
        vec![],
        None,
        128,
        tx,
        workspace.to_string_lossy().into_owned(),
        ReasoningMode::Standard,
    )
    .with_conv_id("bundle-fixture".into())
    .with_permission_manager(manager.clone())
    .with_search_config_path(paths.data_dir.join("config.json"));
    let result = executor
        .execute_skill_tool(&json!({"skill":"dataviz","args":"literal !`echo should-not-run`"}))
        .await
        .unwrap();
    let prompt = result["instructions"].as_str().unwrap();
    assert!(prompt.contains("literal !`echo should-not-run`"));
    let root = prompt
        .lines()
        .next()
        .unwrap()
        .strip_prefix("Base directory for this skill: ")
        .unwrap();
    let path = Path::new(root).join("scripts/validate_palette.js");
    let input = json!({"file_path":path,"offset":2,"limit":3});
    let (_, denied, failed) = executor
        .execute_tool_call("Read", &input, "denied-asset")
        .await;
    assert!(failed, "{denied}");
    manager.set_session_rules("bundle-fixture", std::collections::HashMap::new());
    let (_, allowed, failed) = executor
        .execute_tool_call("Read", &input, "allowed-asset")
        .await;
    assert!(!failed, "{allowed}");
    let result: Value = serde_json::from_str(&allowed).unwrap();
    assert!(result["content"].as_str().unwrap().starts_with("     2\t"));
    assert_eq!(result["content"].as_str().unwrap().lines().count(), 3);
    // A registered file does not open its containing directory to ordinary tools.
    assert!(crate::tools::execute_tool(
        "Write",
        json!({"file_path":path,"content":"changed"}),
        workspace.to_str().unwrap()
    )
    .is_err());
    drop(executor);
    assert!(!path.exists());
}
