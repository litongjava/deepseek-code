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

    let fetch = |conversation: &str, url: &str| PermissionContext {
        mcp_read_only: false,
        tool_name: "WebFetch".into(),
        tool_input: serde_json::json!({"url": url}),
        conversation_id: conversation.into(),
        user_id: None,
        workspace_path: None,
    };
    // Bypass grants every local or private address without Full Access.
    for url in [
        "http://localhost:3000/",
        "http://192.168.1.2/",
        "http://169.254.169.254/latest/meta-data/",
    ] {
        assert_eq!(
            manager.check_permission(&fetch("fetch", url)),
            PermissionResult::Granted,
            "{url}"
        );
    }
    manager.set_session_rules(
        "fetch",
        HashMap::from([("WebFetch".into(), PermissionAction::Deny)]),
    );
    assert!(matches!(
        manager.check_permission(&fetch("fetch", "http://localhost:3000/")),
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
    // Full Access still honors an HTTP deny, without changing HTTPS behavior.
    assert!(matches!(
        manager.check_permission(&fetch("fetch", "http://localhost:3000/")),
        PermissionResult::Denied(_)
    ));
    assert_eq!(
        manager.check_permission(&fetch("fetch", "https://localhost:3000/")),
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
    // Full Access never grants a local fetch in other modes.
    assert!(matches!(
        manager.check_permission(&fetch("ask", "http://localhost:3000/")),
        PermissionResult::RequiresConfirmation(_)
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
