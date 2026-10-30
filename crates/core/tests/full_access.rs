//! Full Access is process-wide, so it is tested in its own binary.
use claude_core::permissions::{
    rules::PermissionAction, AuditLogger, PermissionContext, PermissionManager, PermissionMode,
    PermissionResult,
};
use std::{collections::HashMap, sync::Arc};

#[test]
fn full_access_ignores_deny_rules_and_the_sandbox_only_in_bypass_mode() {
    let manager = PermissionManager::new(Arc::new(AuditLogger::new(16)));
    let context = PermissionContext {
        tool_name: "Write".into(),
        tool_input: serde_json::json!({"path": "test"}),
        conversation_id: "one".into(),
        user_id: None,
        workspace_path: None,
    };
    manager.set_session_rules("one", HashMap::from([("Write".into(), PermissionAction::Deny)]));
    manager.set_mode(PermissionMode::BypassPermissions);
    assert!(matches!(manager.check_permission(&context), PermissionResult::Denied(_)));

    let temp = tempfile::tempdir().unwrap();
    let settings = serde_json::json!({"enabled": true});
    assert!(claude_core::tools::sandbox::policy(&settings, temp.path(), temp.path(), &[]).is_some());

    claude_core::permissions::set_full_access(true);
    assert_eq!(manager.check_permission(&context), PermissionResult::Granted);
    assert!(claude_core::tools::sandbox::policy(&settings, temp.path(), temp.path(), &[]).is_none());
    // Outside bypass mode the deny rule applies again.
    manager.set_mode(PermissionMode::AskPermissions);
    assert!(matches!(manager.check_permission(&context), PermissionResult::Denied(_)));
    claude_core::permissions::set_full_access(false);
}
