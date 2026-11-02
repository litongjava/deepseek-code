//! Full Access is process-wide, so it is tested in its own binary.
use deepseek_code_core::permissions::{
    rules::PermissionAction, AuditLogger, PermissionContext, PermissionManager, PermissionMode,
    PermissionResult,
};
use std::{collections::HashMap, sync::Arc};

#[test]
fn full_access_ignores_deny_rules_and_the_sandbox_only_in_bypass_mode() {
    let manager = PermissionManager::new(Arc::new(AuditLogger::new(16)));
    let context = PermissionContext {
        mcp_read_only: false,
        tool_name: "Write".into(),
        tool_input: serde_json::json!({"path": "test"}),
        conversation_id: "one".into(),
        user_id: None,
        workspace_path: None,
    };
    manager.set_session_rules(
        "one",
        HashMap::from([("Write".into(), PermissionAction::Deny)]),
    );
    manager.set_mode(PermissionMode::BypassPermissions);
    assert!(matches!(
        manager.check_permission(&context),
        PermissionResult::Denied(_)
    ));

    let temp = tempfile::tempdir().unwrap();
    let settings = serde_json::json!({"enabled": true});
    assert!(deepseek_code_core::tools::sandbox::effective_policy(
        &settings,
        temp.path(),
        temp.path(),
        &[],
        manager.get_mode()
    )
    .is_some());

    deepseek_code_core::permissions::set_full_access(true);
    assert_eq!(
        manager.check_permission(&context),
        PermissionResult::Granted
    );
    assert!(deepseek_code_core::tools::sandbox::effective_policy(
        &settings,
        temp.path(),
        temp.path(),
        &[],
        manager.get_mode()
    )
    .is_none());
    // Outside bypass mode the deny rule applies again.
    manager.set_mode(PermissionMode::AskPermissions);
    assert!(matches!(
        manager.check_permission(&context),
        PermissionResult::Denied(_)
    ));
    assert!(deepseek_code_core::tools::sandbox::effective_policy(
        &settings,
        temp.path(),
        temp.path(),
        &[],
        manager.get_mode()
    )
    .is_some());
    manager.set_mode(PermissionMode::PlanMode);
    assert!(deepseek_code_core::tools::sandbox::effective_policy(
        &settings,
        temp.path(),
        temp.path(),
        &[],
        manager.get_mode()
    )
    .is_some());
    deepseek_code_core::permissions::set_full_access(false);
}
