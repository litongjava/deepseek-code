use super::*;

#[test]
fn mcp_server_denials_cover_its_tools_without_matching_other_servers_or_native_tools() {
    for pattern in ["mcp__fixture", "mcp__fixture__*"] {
        let manager = PermissionManager::new(Arc::new(super::super::AuditLogger::new(16)));
        manager.set_mode(PermissionMode::BypassPermissions);
        manager
            .set_launch_rules(
                "session",
                HashMap::from([(pattern.into(), PermissionAction::Deny)]),
            )
            .unwrap();
        for (tool, denied) in [
            ("mcp__fixture__echo", true),
            ("mcp__fixture__with.dot", true),
            ("mcp__fixture__tool__part", true),
            ("mcp__fixture2__echo", false),
            ("Write", false),
        ] {
            let context = PermissionContext {
                mcp_read_only: false,
                tool_name: tool.into(),
                tool_input: serde_json::json!({}),
                conversation_id: "session".into(),
                user_id: None,
                workspace_path: None,
            };
            assert_eq!(
                matches!(
                    manager.check_permission(&context),
                    PermissionResult::Denied(_)
                ),
                denied,
                "{pattern}: {tool}"
            );
        }
    }
    for bad in ["mcp____*", "mcp__fixture*", "mcp__fixture__t*", "Write*"] {
        assert!(super::super::patterns::validate(bad).is_err(), "{bad}");
    }
}

#[test]
fn launch_rules_survive_sdk_patches_and_fork_without_persisting_or_resetting_epoch() {
    let manager = Arc::new(PermissionManager::new(Arc::new(
        super::super::AuditLogger::new(16),
    )));
    manager.set_mode(PermissionMode::BypassPermissions);
    let rules = HashMap::from([
        ("Write".into(), PermissionAction::Deny),
        ("Bash(git status *)".into(), PermissionAction::Allow),
    ]);
    manager.set_launch_rules("parent", rules.clone()).unwrap();
    let revision = manager.policy_snapshot();
    manager.set_launch_rules("parent", rules).unwrap();
    assert_eq!(revision, manager.policy_snapshot());
    let context = PermissionContext {
        mcp_read_only: false,
        tool_name: "Write".into(),
        tool_input: serde_json::json!({"file_path":"output.txt","content":"blocked"}),
        conversation_id: "parent".into(),
        user_id: None,
        workspace_path: None,
    };
    assert!(matches!(
        manager.check_permission(&context),
        PermissionResult::Denied(_)
    ));
    let updates =
        crate::services::permission_updates::Updates::new(manager.clone(), "parent".into());
    updates.apply(&serde_json::json!([
        {"type":"replaceRules","destination":"session","behavior":"deny","rules":[]},
        {"type":"addRules","destination":"session","behavior":"allow","rules":[{"toolName":"Write"}]}
    ])).unwrap();
    assert!(matches!(
        manager.check_permission(&context),
        PermissionResult::Denied(_)
    ));
    updates.clear();
    assert!(matches!(
        manager.check_permission(&context),
        PermissionResult::Denied(_)
    ));
    let child = manager.fork_session("parent", "child");
    manager.set_launch_rules("parent", HashMap::new()).unwrap();
    assert_eq!(
        manager.check_permission(&context),
        PermissionResult::Granted
    );
    let child_context = PermissionContext {
        conversation_id: "child".into(),
        ..context.clone()
    };
    assert!(matches!(
        child.check_permission(&child_context),
        PermissionResult::Denied(_)
    ));
    child
        .set_launch_rules(
            "child",
            HashMap::from([("Write".into(), PermissionAction::Allow)]),
        )
        .unwrap();
    child.set_mode(PermissionMode::PlanMode);
    assert!(matches!(
        child.check_permission(&child_context),
        PermissionResult::Denied(_)
    ));
    // A fresh host never receives the previous process's launch restrictions.
    let fresh = PermissionManager::new(Arc::new(super::super::AuditLogger::new(16)));
    fresh.set_mode(PermissionMode::BypassPermissions);
    assert_eq!(fresh.check_permission(&context), PermissionResult::Granted);
}

#[test]
fn launch_rule_validation_is_atomic_and_scoped_allow_respects_other_denials() {
    let manager = PermissionManager::new(Arc::new(super::super::AuditLogger::new(16)));
    manager.set_mode(PermissionMode::AskPermissions);
    manager
        .set_launch_rules(
            "session",
            HashMap::from([("Bash(git status *)".into(), PermissionAction::Allow)]),
        )
        .unwrap();
    let mut context = PermissionContext {
        mcp_read_only: false,
        tool_name: "Bash".into(),
        tool_input: serde_json::json!({"command":"git status --short"}),
        conversation_id: "session".into(),
        user_id: None,
        workspace_path: None,
    };
    assert_eq!(
        manager.check_permission(&context),
        PermissionResult::Granted
    );
    let revision = manager.policy_snapshot();
    assert!(manager
        .set_launch_rules(
            "session",
            HashMap::from([
                ("Write".into(), PermissionAction::Allow),
                ("Read(../secret)".into(), PermissionAction::Allow)
            ])
        )
        .is_err());
    assert_eq!(manager.policy_snapshot(), revision);
    assert_eq!(
        manager.check_permission(&context),
        PermissionResult::Granted
    );
    context.tool_input = serde_json::json!({"command":"git push"});
    assert!(matches!(
        manager.check_permission(&context),
        PermissionResult::RequiresConfirmation(_)
    ));
    manager.set_session_rules(
        "session",
        HashMap::from([("Bash".into(), PermissionAction::Deny)]),
    );
    context.tool_input = serde_json::json!({"command":"git status --short"});
    assert!(matches!(
        manager.check_permission(&context),
        PermissionResult::Denied(_)
    ));
}
