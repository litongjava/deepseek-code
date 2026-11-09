use super::*;

fn executor(root: &std::path::Path) -> ToolLoopExecutor {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let (tx, _rx) = mpsc::channel(32);
    let provider = ResolvedProvider {
        provider: serde_json::from_value(json!({"id":"hook-test","name":"hook-test","base_url":"http://127.0.0.1:1","api_key":"unused","api_format":"openai","models":[],"enabled":true})).unwrap(),
        model: serde_json::from_value(json!({"id":"test","name":"test","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap(),
    };
    ToolLoopExecutor::new(
        provider,
        vec![],
        None,
        128,
        tx,
        root.to_string_lossy().into_owned(),
        ReasoningMode::Standard,
    )
    .with_hooks(crate::services::tool_hooks::HookRunner::load(root, root).unwrap())
}

fn write_hook(root: &std::path::Path, event: &str, command: &str) {
    std::fs::write(root.join("settings.json"), json!({"hooks":{event:[{"matcher":"Write","hooks":[{"type":"command","command":command,"shell":if cfg!(windows){"powershell"}else{"sh"}}]}]}}).to_string()).unwrap();
}

#[tokio::test]
async fn pre_tool_hook_blocks_real_write_before_execution() {
    let root = tempfile::tempdir().unwrap();
    write_hook(root.path(), "PreToolUse", "exit 2");
    let mut executor = executor(root.path());
    let (_, output, failed) = executor
        .execute_tool_call(
            "Write",
            &json!({"file_path":"blocked.txt","content":"must not exist"}),
            "test",
        )
        .await;
    assert!(failed, "{output}");
    assert!(output.contains("hook"), "{output}");
    assert!(!root.path().join("blocked.txt").exists());
}

#[tokio::test]
async fn bypass_skips_hook_approval_but_preserves_hook_denial() {
    for decision in ["ask", "deny"] {
        let root = tempfile::tempdir().unwrap();
        let output = json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":decision}}).to_string();
        let command = if cfg!(windows) {
            format!("[Console]::Out.Write('{output}')")
        } else {
            format!("printf '%s' '{output}'")
        };
        write_hook(root.path(), "PreToolUse", &command);
        let manager = crate::runtime::default_permissions();
        manager.set_mode(crate::permissions::PermissionMode::BypassPermissions);
        let mut executor = executor(root.path())
            .with_permission_manager(manager)
            .with_permission_host(false);
        let (events, mut received) = mpsc::channel(32);
        executor.event_tx = events;
        let (_, output, failed) = executor
            .execute_tool_call(
                "Write",
                &json!({"file_path":"allowed.txt","content":"written"}),
                "hook-bypass",
            )
            .await;
        assert_eq!(failed, decision == "deny", "{output}");
        assert_eq!(root.path().join("allowed.txt").exists(), decision == "ask");
        while let Ok(event) = received.try_recv() {
            assert!(!matches!(
                event,
                EngineEvent::ToolPermission { .. } | EngineEvent::AskUser { .. }
            ));
        }
    }
}

#[tokio::test]
async fn modified_input_is_executed_and_post_context_reaches_tool_result() {
    let root = tempfile::tempdir().unwrap();
    let hook_output = r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","updatedInput":{"file_path":"actual.txt","content":"hook content"}}}"#;
    let command = if cfg!(windows) {
        format!("[Console]::Out.Write('{hook_output}')")
    } else {
        format!("printf '%s' '{hook_output}'")
    };
    write_hook(root.path(), "PreToolUse", &command);
    let path = root.path().join("settings.json");
    let mut config: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let post_output = r#"{"hookSpecificOutput":{"hookEventName":"PostToolUse","additionalContext":"Run formatter before completion"}}"#;
    let post_command = if cfg!(windows) {
        format!("[Console]::Out.Write('{post_output}')")
    } else {
        format!("printf '%s' '{post_output}'")
    };
    config["hooks"]["PostToolUse"] = json!([{"matcher":"Write","hooks":[{"type":"command","command":post_command,"shell":if cfg!(windows){"powershell"}else{"sh"}}]}]);
    std::fs::write(path, config.to_string()).unwrap();
    let mut executor = executor(root.path());
    let (actual, output, failed) = executor
        .execute_tool_call(
            "Write",
            &json!({"file_path":"original.txt","content":"original"}),
            "test",
        )
        .await;
    assert!(!failed, "{output}");
    assert!(
        output.contains("Run formatter before completion"),
        "{output}"
    );
    assert_eq!(actual["file_path"], "actual.txt");
    assert!(!root.path().join("original.txt").exists());
    assert_eq!(
        std::fs::read_to_string(root.path().join("actual.txt")).unwrap(),
        "hook content"
    );
}

#[tokio::test]
async fn stop_hook_obeys_shared_default_consecutive_block_cap() {
    let root = tempfile::tempdir().unwrap();
    write_hook(root.path(), "Stop", "exit 2");
    let mut executor = executor(root.path());
    for _ in 0..8 {
        assert!(executor.stop_hook("done").await.unwrap().is_some());
    }
    assert!(executor.stop_hook("still done").await.unwrap().is_none());
}

#[tokio::test]
async fn ordinary_stop_hooks_and_goal_checks_consume_one_session_budget() {
    use crate::services::session_goals as goals;
    let root = tempfile::tempdir().unwrap();
    write_hook(root.path(), "Stop", "exit 2");
    let db = Arc::new(crate::db::DbManager::new(root.path().join("state.db")).unwrap());
    db.init().unwrap();
    let goal = goals::set(&db, "parent", "Finish the task").unwrap();
    for _ in 0..3 {
        assert_eq!(
            goals::block(&db, "parent", Some(&goal.id), 8).unwrap(),
            Some(true)
        );
    }
    let mut parent = executor(root.path())
        .with_conv_id("parent".into())
        .with_goal_state(db.clone());
    for _ in 0..5 {
        assert!(parent.stop_hook("done").await.unwrap().is_some());
    }
    assert!(parent.stop_hook("done").await.unwrap().is_none());
    assert_eq!(
        goals::active(&db, "parent").unwrap().unwrap().state,
        "paused"
    );
    let mut child = executor(root.path())
        .with_conv_id("child".into())
        .with_goal_state(db.clone());
    assert!(child.stop_hook("child done").await.unwrap().is_some());
    assert_eq!(
        goals::active(&db, "parent").unwrap().unwrap().stop_blocks,
        9
    );
}

#[tokio::test]
async fn stop_hook_prevent_continuation_pauses_goal_before_outer_evaluation() {
    use crate::services::session_goals as goals;
    let root = tempfile::tempdir().unwrap();
    let output = r#"{"continue":false,"stopReason":"User hook stopped work"}"#;
    let command = if cfg!(windows) {
        format!("[Console]::Out.Write('{output}')")
    } else {
        format!("printf '%s' '{output}'")
    };
    write_hook(root.path(), "Stop", &command);
    let db = Arc::new(crate::db::DbManager::new(root.path().join("state.db")).unwrap());
    db.init().unwrap();
    let goal = goals::set(&db, "parent", "Keep working").unwrap();
    let mut parent = executor(root.path())
        .with_conv_id("parent".into())
        .with_goal_state(db.clone());
    assert!(parent.stop_hook("done").await.unwrap().is_none());
    assert_eq!(
        goals::active(&db, "parent").unwrap().unwrap().state,
        "paused"
    );
    assert!(!goals::verdict(&db, "parent", &goal.id, "continue", "late check").unwrap());
}

#[tokio::test]
async fn task_created_and_completed_hooks_block_and_roll_back() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    let shell = if cfg!(windows) { "powershell" } else { "sh" };
    let make = executor;
    // The task tools are on demand by default; this test drives them directly.
    crate::services::customization::save(
        &data,
        &crate::services::customization::Customization {
            tool_loading: ["TaskCreate", "TaskUpdate", "TaskGet", "TaskList"]
                .into_iter()
                .map(|name| {
                    (
                        name.to_owned(),
                        crate::services::customization::ToolLoading {
                            enabled: true,
                            on_demand: true,
                        },
                    )
                })
                .collect(),
            ..Default::default()
        },
        &[],
    )
    .unwrap();
    std::fs::write(
        temp.path().join("settings.json"),
        json!({"hooks":{"TaskCreated":[{"hooks":[{"type":"command","command":"exit 2","shell":shell}]}]}})
            .to_string(),
    )
    .unwrap();
    let mut executor = executor(temp.path())
        .with_conv_id(session.clone())
        .with_search_config_path(data.join("settings.json"));
    let (_, output, failed) = executor
        .execute_tool_call("TaskCreate", &json!({"subject":"blocked task"}), "create")
        .await;
    assert!(failed, "{output}");
    assert!(output.contains("TaskCreated hook"), "{output}");
    let (_, listed, _) = executor
        .execute_tool_call("TaskList", &json!({}), "list")
        .await;
    assert!(!listed.contains("blocked task"), "{listed}");

    std::fs::write(
        temp.path().join("settings.json"),
        json!({"hooks":{"TaskCompleted":[{"hooks":[{"type":"command","command":"exit 2","shell":shell}]}]}})
            .to_string(),
    )
    .unwrap();
    // HookRunner snapshots settings at load time.
    let mut executor = make(temp.path())
        .with_conv_id(session)
        .with_search_config_path(data.join("settings.json"));
    let (_, created, failed) = executor
        .execute_tool_call("TaskCreate", &json!({"subject":"kept task"}), "create")
        .await;
    assert!(!failed, "{created}");
    let task: Value = serde_json::from_str(&created).unwrap();
    let id = task["id"].as_str().unwrap().to_owned();
    let (_, blocked, failed) = executor
        .execute_tool_call(
            "TaskUpdate",
            &json!({"id":id,"status":"completed"}),
            "update",
        )
        .await;
    assert!(!failed, "{blocked}");
    assert!(blocked.contains("TaskCompleted hook"), "{blocked}");
    let (_, read, _) = executor
        .execute_tool_call("TaskGet", &json!({"id":id}), "get")
        .await;
    let task: Value = serde_json::from_str(&read).unwrap();
    assert_eq!(task["status"], "pending");
}
