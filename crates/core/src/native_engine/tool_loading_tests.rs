use super::*;

fn provider() -> ResolvedProvider {
    ResolvedProvider {
        provider: serde_json::from_value(json!({"id":"fixture","name":"fixture","base_url":"http://127.0.0.1:1","api_key":"unused","api_format":"openai","models":[],"enabled":true})).unwrap(),
        model: serde_json::from_value(json!({"id":"fixture","name":"fixture","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap(),
    }
}

fn executor(root: &std::path::Path) -> ToolLoopExecutor {
    let _ = crate::config::OrchestrationConfig::init_global();
    let (tx, _rx) = mpsc::channel(64);
    let manager = crate::runtime::default_permissions();
    manager.set_mode(crate::permissions::PermissionMode::BypassPermissions);
    ToolLoopExecutor::new(
        provider(),
        vec![],
        None,
        128,
        tx,
        root.to_string_lossy().into_owned(),
        ReasoningMode::Standard,
    )
    .with_conv_id("tool-loading-fixture".into())
    .with_permission_manager(manager)
    .with_search_config_path(
        crate::data_layout::DataLayout::from_root(&root).config_file("config.json"),
    )
}

fn write_switches(root: &std::path::Path, tools: &[(&str, bool, bool)]) {
    let customization = crate::services::customization::Customization {
        tool_loading: tools
            .iter()
            .map(|(name, enabled, on_demand)| {
                (
                    (*name).to_owned(),
                    crate::services::customization::ToolLoading {
                        enabled: *enabled,
                        on_demand: *on_demand,
                    },
                )
            })
            .collect(),
        ..Default::default()
    };
    crate::services::customization::save(root, &customization, &[]).unwrap();
}

fn names(executor: &ToolLoopExecutor) -> Vec<String> {
    executor
        .tool_definitions()
        .unwrap()
        .into_iter()
        .map(|tool| tool.name)
        .collect()
}

/// The loadable-tool reminder this session's first user message carries. The
/// system prompt no longer holds the list.
fn loadable_reminder(executor: &ToolLoopExecutor) -> Option<String> {
    let customization = executor.customization().unwrap();
    let catalog = customization.described(executor.builtin_catalog());
    let (builtin, mcp) = crate::native_engine::tool_loop::loadable_tool_lists(
        &customization,
        &catalog,
        &executor.mcp_tools,
        &executor.mcp_states,
        executor.allowed_tools.as_deref(),
    );
    crate::prompt::loadable_tools_reminder(&builtin, &mcp)
}


/// A fake MCP tool, set on the executor directly: discovery needs a running
/// server, which these tests do not start.
fn mcp_tool(name: &str, description: &str) -> crate::tools::ToolDefinition {
    crate::tools::ToolDefinition {
        summary: None,
        name: name.into(),
        description: description.into(),
        input_schema: json!({"type":"object"}),
    }
}

#[test]
fn the_request_carries_only_the_core_tools_by_default() {
    let temp = tempfile::tempdir().unwrap();
    let executor = executor(temp.path());
    let names = names(&executor);
    for name in [
        "Bash",
        "Read",
        "Write",
        "Edit",
        "MultiEdit",
        "Grep",
        "Glob",
        "Skill",
        "ToolLoad",
        "AskUserQuestion",
        "EnterPlanMode",
        "ExitPlanMode",
        "TaskGet",
        "TaskStop",
        "WebFetch",
        "WebSearch",
    ] {
        assert!(
            names.contains(&name.to_owned()),
            "{name} missing: {names:?}"
        );
    }
    for name in [
        "ListDir",
        "Monitor",
        "NotebookEdit",
        "computer_use",
        "ContextSearch",
        "Agent",
        "Workflow",
    ] {
        assert!(
            !names.contains(&name.to_owned()),
            "{name} stays on demand: {names:?}"
        );
    }
}

#[test]
fn a_non_interactive_executor_is_not_offered_ask_user_question() {
    let temp = tempfile::tempdir().unwrap();
    let executor = executor(temp.path()).with_permission_host(false);
    let names = names(&executor);
    assert!(
        !names.contains(&"AskUserQuestion".to_owned()),
        "the tool can only time out here: {names:?}"
    );
    assert!(
        !names.contains(&"ToolLoad".to_owned())
            || loadable_reminder(&executor)
                .is_none_or(|reminder| !reminder.contains("AskUserQuestion")),
        "ToolLoad must not offer the question tool: {:?}",
        loadable_reminder(&executor)
    );
    let prompt = executor.request_system_prompt().unwrap();
    assert!(prompt.contains("# Interactive user"));
    assert!(prompt.contains("- Interactive user: no (do not call AskUserQuestion)"));
}

#[tokio::test]
async fn context_search_follows_loading_settings_and_session_scope() {
    use crate::runtime::{RuntimePaths, SessionStore};

    let temp = tempfile::tempdir().unwrap();
    let no_storage = executor(temp.path());
    let request = no_storage.prepare_request().unwrap();
    assert!(!request.system_prompt.unwrap_or_default().contains("ContextSearch"));
    assert!(
        !loadable_reminder(&no_storage)
            .unwrap_or_default()
            .contains("ContextSearch"),
        "without session storage ContextSearch is not in the catalog"
    );

    let db = RuntimePaths::new(temp.path().join("session-store"))
        .unwrap()
        .open_database()
        .unwrap();
    let store = SessionStore::new(db.clone());
    let session = store.create(temp.path(), "fixture", "fixture").unwrap();
    let message_id = store
        .append(&session, "user", "old context needle")
        .unwrap();
    let mut executor = no_storage.with_conv_id(session).with_goal_state(db);
    let request = executor.prepare_request().unwrap();
    executor.exposed_tools = Some(request.tools.iter().map(|tool| tool.name.clone()).collect());
    assert!(!names(&executor).iter().any(|name| name == "ContextSearch"));
    let prompt = request.system_prompt.unwrap_or_default();
    assert!(!prompt.contains("ContextSearch"), "{prompt}");
    let listing = loadable_reminder(&executor).unwrap();
    assert!(listing.contains("- ContextSearch:"), "{listing}");
    assert!(!listing.contains("After compaction, load ContextSearch"), "{listing}");

    let input = json!({"query":"needle"});
    let (_, output, failed) = executor
        .execute_tool_call("ContextSearch", &input, "early")
        .await;
    assert!(failed && output.contains("not loaded"), "{output}");
    let (_, output, failed) = executor
        .execute_tool_call(
            "ToolLoad",
            &json!({"names":["ContextSearch"]}),
            "load",
        )
        .await;
    assert!(!failed, "{output}");
    let result: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(result["loaded_tools"], json!(["ContextSearch"]));
    assert_eq!(
        result["tools"][0]["input_schema"]["required"],
        json!(["query"])
    );

    let (_, output, failed) = executor
        .execute_tool_call("ContextSearch", &input, "same-request")
        .await;
    assert!(failed && output.contains("next model request"), "{output}");
    let request = executor.prepare_request().unwrap();
    executor.exposed_tools = Some(request.tools.iter().map(|tool| tool.name.clone()).collect());
    assert!(!request
        .system_prompt
        .unwrap_or_default()
        .contains("After compaction, use ContextSearch"));
    let (_, output, failed) = executor
        .execute_tool_call("ContextSearch", &input, "search")
        .await;
    assert!(!failed, "{output}");
    let result: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(result["matches"][0]["message_id"], message_id);

    use crate::permissions::{PermissionLevel, PermissionMode, PermissionScope, ToolPermission};
    let manager = executor.permission_manager.as_ref().unwrap();
    manager.set_mode(PermissionMode::AskPermissions);
    manager.set_tool_permission(
        "ContextSearch",
        ToolPermission {
            tool_name: "ContextSearch".into(),
            allowed: false,
            requires_confirmation: false,
            scope: PermissionScope::Global,
            level: PermissionLevel::None,
        },
    );
    let (_, output, failed) = executor
        .execute_tool_call("ContextSearch", &input, "denied")
        .await;
    assert!(failed && output.contains("Permission denied"), "{output}");

    write_switches(temp.path(), &[("ContextSearch", false, false)]);
    let request = executor.prepare_request().unwrap();
    assert!(!request.system_prompt.unwrap_or_default().contains("ContextSearch"));
    let (_, output, failed) = executor
        .execute_tool_call("ContextSearch", &input, "off")
        .await;
    assert!(failed && output.contains("turned off"), "{output}");
}

#[test]
fn enabling_a_tool_adds_it_to_the_request_without_changing_the_other_switch() {
    let temp = tempfile::tempdir().unwrap();
    // A core tool that was turned off: the file has no override, so the default
    // would put it back. The explicit entry wins.
    write_switches(
        temp.path(),
        &[("Grep", false, true), ("WebFetch", true, false)],
    );
    let executor = executor(temp.path());
    let names = names(&executor);
    assert!(!names.contains(&"Grep".to_owned()), "{names:?}");
    assert!(names.contains(&"WebFetch".to_owned()), "{names:?}");
    let customization = executor.customization().unwrap();
    assert_eq!(
        customization.loading("WebFetch"),
        crate::services::customization::ToolLoading {
            enabled: true,
            on_demand: false
        }
    );
    assert_eq!(
        customization.loading("Grep"),
        crate::services::customization::ToolLoading {
            enabled: false,
            on_demand: true
        }
    );
}

#[tokio::test]
async fn tool_load_loads_the_real_definition_and_the_tool_runs_after_the_next_request() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("sample.txt"), "haystack").unwrap();
    let mut executor = executor(temp.path());
    let exposed: std::collections::HashSet<String> = names(&executor).into_iter().collect();
    executor.exposed_tools = Some(exposed);

    let (_, output, failed) = executor
        .execute_tool_call("ToolLoad", &json!({"names":["ListDir"]}), "search")
        .await;
    assert!(!failed, "{output}");
    let value: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(value["success"], true);
    assert_eq!(value["loaded_tools"], json!(["ListDir"]));
    assert!(
        value["tools"][0]["input_schema"]["properties"]["path"].is_object(),
        "{output}"
    );

    // The same batch was already prepared without it.
    let (_, output, failed) = executor
        .execute_tool_call("ListDir", &json!({"path":"."}), "list")
        .await;
    assert!(failed, "{output}");
    assert!(output.contains("next model request"), "{output}");

    // The next request carries the definition, and the call then runs.
    let exposed: std::collections::HashSet<String> = names(&executor).into_iter().collect();
    assert!(exposed.contains("ListDir"));
    executor.exposed_tools = Some(exposed);
    let (_, output, failed) = executor
        .execute_tool_call("ListDir", &json!({"path":"."}), "list")
        .await;
    assert!(!failed, "{output}");
    assert!(output.contains("sample.txt"), "{output}");
}

#[tokio::test]
async fn a_tool_with_both_switches_off_is_neither_discoverable_nor_callable() {
    let temp = tempfile::tempdir().unwrap();
    write_switches(temp.path(), &[("computer_use", false, false)]);
    let mut executor = executor(temp.path());
    let (_, output, _) = executor
        .execute_tool_call("ToolLoad", &json!({"names":["computer_use"]}), "search")
        .await;
    // An explicit name is reported with its reason instead of a generic miss.
    assert!(output.contains("disabled by tool settings"), "{output}");
    let value: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(value["success"], false);
    assert_eq!(value["loaded_tools"], json!([]));
    assert_eq!(
        value["unavailable"],
        json!([{"name":"computer_use","reason":"disabled by tool settings"}])
    );
    let (_, output, failed) = executor
        .execute_tool_call("computer_use", &json!({"action_type":"Screenshot"}), "use")
        .await;
    assert!(failed, "{output}");
    assert!(output.contains("turned off"), "{output}");
}

#[tokio::test]
async fn tool_load_rejects_invalid_input_and_uses_a_bounded_array_schema() {
    let root = tempfile::tempdir().unwrap();
    let mut executor = executor(root.path());
    let definition = executor
        .tool_definitions()
        .unwrap()
        .into_iter()
        .find(|tool| tool.name == "ToolLoad")
        .unwrap();
    let schema = &definition.input_schema;
    assert_eq!(schema["required"], json!(["names"]));
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(schema["properties"]["names"]["type"], "array");
    assert_eq!(schema["properties"]["names"]["items"]["type"], "string");
    assert_eq!(schema["properties"]["names"]["minItems"], 1);
    let max = schema["properties"]["names"]["maxItems"].as_u64().unwrap() as usize;
    assert_eq!(max, 64);
    for input in [
        json!({}),
        json!({"query":"ListDir"}),
        json!({"names":"ListDir"}),
        json!({"names":[]}),
        json!({"names":[""]}),
        json!({"names":["ListDir", 7]}),
        json!({"names":["ListDir"], "extra": true}),
        json!({"names":vec!["ListDir"; max + 1]}),
    ] {
        let (_, output, failed) = executor
            .execute_tool_call("ToolLoad", &input, "invalid")
            .await;
        assert!(failed && output.contains("ToolLoad"), "{input}: {output}");
        assert!(executor.loaded_tools.is_empty(), "{input}");
    }
}

#[test]
fn only_a_paired_tool_load_result_restores_the_tools_of_this_session() {
    let temp = tempfile::tempdir().unwrap();
    let restore = |messages: Vec<Value>| {
        let mut executor = executor(temp.path());
        executor.messages = messages;
        executor.restore_loaded_tools();
        names(&executor)
    };
    let loaded = |tools: &[&str]| json!({"success":true,"loaded_tools":tools,"tools":[]});
    let openai_call = |id: &str, name: &str| json!({"role":"assistant","content":"","tool_calls":[{"id":id,"type":"function","function":{"name":name,"arguments":"{}"}}]});
    let openai_result = |id: &str, body: &Value| json!({"role":"tool","tool_call_id":id,"content":body.to_string()});

    // A real ToolLoad call and its matching result.
    assert!(restore(vec![
        openai_call("c1", "ToolLoad"),
        openai_result("c1", &loaded(&["Monitor"]))
    ])
    .contains(&"Monitor".to_owned()));

    // The Anthropic shape read the same way.
    let anthropic = vec![
        json!({"role":"assistant","content":[{"type":"tool_use","id":"a1","name":"ToolLoad","input":{"names":["ListDir"]}}]}),
        json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"a1","content":loaded(&["ListDir"]).to_string()}]}),
    ];
    assert!(restore(anthropic).contains(&"ListDir".to_owned()));

    // An error result never loads, even with the right pairing.
    let errored = vec![
        json!({"role":"assistant","content":[{"type":"tool_use","id":"a2","name":"ToolLoad","input":{}}]}),
        json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"a2","is_error":true,"content":loaded(&["Monitor"]).to_string()}]}),
    ];
    assert!(!restore(errored).contains(&"Monitor".to_owned()));

    // An orphan result, a mismatched id, another tool's result and a user's own
    // JSON each fail to grant anything.
    assert!(!restore(vec![openai_result("c1", &loaded(&["Monitor"]))])
        .contains(&"Monitor".to_owned()));
    assert!(!restore(vec![
        openai_call("c1", "ToolLoad"),
        openai_result("c2", &loaded(&["Monitor"]))
    ])
    .contains(&"Monitor".to_owned()));
    assert!(!restore(vec![
        openai_call("c1", "Glob"),
        openai_result("c1", &loaded(&["Monitor"]))
    ])
    .contains(&"Monitor".to_owned()));
    assert!(!restore(vec![
        json!({"role":"user","content":loaded(&["Monitor"]).to_string()})
    ])
    .contains(&"Monitor".to_owned()));

    // A tool switched off after it was loaded does not come back.
    write_switches(temp.path(), &[("Monitor", false, false)]);
    assert!(!restore(vec![
        openai_call("c1", "ToolLoad"),
        openai_result("c1", &loaded(&["Monitor"]))
    ])
    .contains(&"Monitor".to_owned()));
}

/// Stored history and old settings cannot resurrect a removed model tool: it is
/// no longer in the catalog, so it is neither restored nor sent, and a direct
/// call is rejected instead of being routed elsewhere.
#[tokio::test]
async fn removed_git_tools_cannot_be_restored_configured_or_called() {
    let temp = tempfile::tempdir().unwrap();
    let removed = ["git_status", "git_diff", "git_log", "git_add", "git_commit", "smart_project_scan"];
    // A stored successful ToolLoad result still names them, alongside a real
    // tool that must keep restoring.
    let stored = ["git_status", "git_add", "ListDir"];
    let mut restored = executor(temp.path());
    restored.messages = vec![
        json!({"role":"assistant","content":"","tool_calls":[{"id":"old","type":"function","function":{"name":"ToolLoad","arguments":"{\"names\":[\"git_status\",\"git_add\"]}"}}]}),
        json!({"role":"tool","tool_call_id":"old","content":json!({"success":true,"loaded_tools":stored,"tools":[]}).to_string()}),
    ];
    restored.restore_loaded_tools();
    let sent = names(&restored);
    for name in removed {
        assert!(!sent.contains(&name.to_owned()), "{name} was restored: {sent:?}");
    }
    assert!(sent.contains(&"ListDir".to_owned()), "{sent:?}");

    // An old settings file that enables them is equally inert.
    write_switches(
        temp.path(),
        &removed.iter().map(|name| (*name, true, false)).collect::<Vec<_>>(),
    );
    let mut executor = executor(temp.path());
    executor.exposed_tools = Some(names(&executor).into_iter().collect());
    let sent = names(&executor);
    for name in removed {
        assert!(!sent.contains(&name.to_owned()), "{name} was sent: {sent:?}");
        let (_, output, failed) = executor
            .execute_tool_call(name, &json!({"path":"."}), "removed")
            .await;
        assert!(failed, "{name}: {output}");
        assert!(output.contains("Unknown tool"), "{name}: {output}");
    }
}

#[tokio::test]
async fn retired_task_output_is_not_advertised_searched_or_restored() {
    let temp = tempfile::tempdir().unwrap();
    let mut probe = executor(temp.path());
    let names = names(&probe);
    assert!(names.contains(&"TaskGet".to_owned()), "{names:?}");
    assert!(!names.contains(&"TaskOutput".to_owned()), "{names:?}");
    assert!(crate::tools::get_tool_definitions()
        .iter()
        .all(|tool| tool.name != "TaskOutput"));
    let exposed: std::collections::HashSet<String> = names.into_iter().collect();
    probe.exposed_tools = Some(exposed);
    let (_, output, failed) = probe
        .execute_tool_call("ToolLoad", &json!({"names":["TaskOutput"]}), "search")
        .await;
    assert!(failed, "{output}");
    let value: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(value["loaded_tools"], json!([]), "{output}");
    assert_eq!(value["tools"], json!([]), "{output}");

    // TaskGet is resident by default, so ToolLoad reports it as already available.
    let (_, output, failed) = probe
        .execute_tool_call("ToolLoad", &json!({"names":["TaskGet"]}), "search")
        .await;
    assert!(!failed, "{output}");
    let value: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(value["already_available"], json!(["TaskGet"]), "{output}");
    assert_eq!(value["loaded_tools"], json!([]), "{output}");

    // A stored search result from before the consolidation cannot restore the
    // retired name either.
    let mut restored = executor(temp.path());
    restored.messages = vec![
        json!({"role":"assistant","content":[{"type":"tool_use","id":"a1","name":"ToolLoad","input":{}}]}),
        json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"a1","content":json!({"success":true,"loaded_tools":["TaskOutput","WebSearch"],"tools":[]}).to_string()}]}),
    ];
    restored.restore_loaded_tools();
    assert!(!restored.loaded_tools.contains("TaskOutput"));
    assert!(restored.loaded_tools.contains("WebSearch"));
}

#[tokio::test]
async fn a_legacy_task_output_call_follows_task_get_switches() {
    let id = uuid::Uuid::new_v4().to_string();
    // Both switches off: the alias must be refused before any dispatch.
    let temp = tempfile::tempdir().unwrap();
    write_switches(temp.path(), &[("TaskGet", false, false)]);
    let mut disabled = executor(temp.path());
    let (_, output, failed) = disabled
        .execute_tool_call("TaskOutput", &json!({"task_id":id}), "alias-off")
        .await;
    assert!(failed, "{output}");
    assert!(output.contains("TaskGet is turned off"), "{output}");

    // On demand but never searched: the alias cannot skip the loading gate.
    let temp = tempfile::tempdir().unwrap();
    write_switches(temp.path(), &[("TaskGet", false, true)]);
    let mut unloaded = executor(temp.path());
    let (_, output, failed) = unloaded
        .execute_tool_call("TaskOutput", &json!({"task_id":id}), "alias-unloaded")
        .await;
    assert!(failed, "{output}");
    assert!(output.contains("TaskGet is not loaded"), "{output}");
}

#[tokio::test]
async fn task_get_can_be_discovered_on_demand_and_invoked() {
    let temp = tempfile::tempdir().unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    write_switches(temp.path(), &[("TaskGet", false, true)]);
    let mut executor = executor(temp.path()).with_conv_id(session.clone());
    let initial = names(&executor);
    assert!(!initial.contains(&"TaskGet".to_owned()), "{initial:?}");
    executor.exposed_tools = Some(initial.into_iter().collect());
    let (_, output, failed) = executor
        .execute_tool_call("ToolLoad", &json!({"names":["TaskGet"]}), "search")
        .await;
    assert!(!failed, "{output}");
    let loaded: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(loaded["success"], true, "{output}");
    assert_eq!(loaded["loaded_tools"], json!(["TaskGet"]), "{output}");

    let task = crate::tools::tasks::execute_for_session(
        "TaskCreate",
        json!({"subject":"hidden task"}),
        temp.path(),
        &session,
    )
    .unwrap();
    let next: std::collections::HashSet<String> = names(&executor).into_iter().collect();
    assert!(next.contains("TaskGet"), "{next:?}");
    executor.exposed_tools = Some(next);
    let (_, output, failed) = executor
        .execute_tool_call("TaskGet", &json!({"id":task["id"]}), "get")
        .await;
    assert!(!failed, "{output}");
    let value: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(value["subject"], "hidden task");
}

#[tokio::test]
async fn task_get_is_resident_by_default() {
    let temp = tempfile::tempdir().unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    let mut executor = executor(temp.path()).with_conv_id(session.clone());
    let initial = names(&executor);
    assert!(initial.contains(&"TaskGet".to_owned()), "{initial:?}");
    executor.exposed_tools = Some(initial.into_iter().collect());

    let task = crate::tools::tasks::execute_for_session(
        "TaskCreate",
        json!({"subject":"resident task"}),
        temp.path(),
        &session,
    )
    .unwrap();
    let (_, output, failed) = executor
        .execute_tool_call("TaskGet", &json!({"id":task["id"]}), "get")
        .await;
    assert!(!failed, "{output}");
    let value: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(value["subject"], "resident task");
}

#[test]
fn the_catalog_carries_each_task_builtin_once_and_no_retired_entries() {
    let catalog = crate::tools::get_tool_definitions();
    for name in ["TaskCreate", "TaskGet", "TaskUpdate", "TaskList"] {
        assert_eq!(
            catalog.iter().filter(|tool| tool.name == name).count(),
            1,
            "{name} must be one real builtin"
        );
    }
    for name in ["TaskOutput", "plan_mode"] {
        assert!(
            catalog.iter().all(|tool| tool.name != name),
            "{name} must stay retired"
        );
    }
    // The retained task capabilities are part of the advertised schemas.
    let update = catalog
        .iter()
        .find(|tool| tool.name == "TaskUpdate")
        .unwrap();
    assert_eq!(
        update.input_schema["properties"]["setBlockedBy"]["type"],
        "array"
    );
    let get = catalog.iter().find(|tool| tool.name == "TaskGet").unwrap();
    assert_eq!(get.input_schema["properties"]["block"]["type"], "boolean");
    assert_eq!(
        get.input_schema["properties"]["timeout"]["maximum"],
        600_000
    );
    // Enter/Exit stay real builtins with their own definitions.
    for name in ["EnterPlanMode", "ExitPlanMode"] {
        assert_eq!(
            catalog.iter().filter(|tool| tool.name == name).count(),
            1,
            "{name} must stay advertised"
        );
    }
}

#[tokio::test]
async fn retired_plan_mode_cannot_be_resurrected_by_settings_loaded_names_or_a_direct_call() {
    let temp = tempfile::tempdir().unwrap();
    // A settings file written before the retirement still says "on".
    write_switches(temp.path(), &[("plan_mode", true, true)]);
    let mut probe = executor(temp.path());
    let names = names(&probe);
    assert!(!names.contains(&"plan_mode".to_owned()), "{names:?}");
    assert!(crate::tools::get_tool_definitions()
        .iter()
        .all(|tool| tool.name != "plan_mode"));

    // It is not searchable either.
    probe.exposed_tools = Some(names.into_iter().collect());
    let (_, output, failed) = probe
        .execute_tool_call("ToolLoad", &json!({"names":["plan_mode"]}), "search")
        .await;
    assert!(failed, "{output}");
    let value: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(value["loaded_tools"], json!([]), "{output}");
    assert_eq!(value["tools"], json!([]), "{output}");

    // An old stored ToolLoad result cannot restore it.
    let mut restored = executor(temp.path());
    restored.messages = vec![
        json!({"role":"assistant","content":[{"type":"tool_use","id":"a1","name":"ToolLoad","input":{}}]}),
        json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"a1","content":json!({"success":true,"loaded_tools":["plan_mode","TaskGet"],"tools":[]}).to_string()}]}),
    ];
    restored.restore_loaded_tools();
    assert!(!restored.loaded_tools.contains("plan_mode"));
    assert!(restored.loaded_tools.contains("TaskGet"));

    // A direct call fails before any legacy plan storage is created, even
    // though this executor has a settings file that enabled the old name.
    std::fs::create_dir_all(temp.path().join("work")).unwrap();
    let mut direct = executor(&temp.path().join("work"));
    let (_, output, failed) = direct
        .execute_tool_call(
            "plan_mode",
            &json!({"operation":"create","task":"legacy","steps":[]}),
            "retired",
        )
        .await;
    assert!(failed, "{output}");
    assert!(output.contains("Unknown tool: plan_mode"), "{output}");
    assert!(!temp.path().join("work/.claude").exists());
    assert_eq!(
        crate::tools::execute_tool(
            "plan_mode",
            json!({"operation":"list"}),
            &temp.path().to_string_lossy()
        )
        .unwrap_err()
        .to_string(),
        "Unknown tool: plan_mode"
    );
}

#[tokio::test]
async fn retired_plan_mode_never_reaches_hooks_or_permission_prompts() {
    let temp = tempfile::tempdir().unwrap();
    let _ = crate::config::OrchestrationConfig::init_global();
    let (tx, mut rx) = mpsc::channel(16);
    let manager = crate::runtime::default_permissions();
    manager.set_mode(crate::permissions::PermissionMode::AskPermissions);
    manager.set_session_rules(
        "tool-loading-fixture",
        std::collections::HashMap::from([(
            "plan_mode".into(),
            crate::permissions::rules::PermissionAction::Confirm,
        )]),
    );
    let mut executor = ToolLoopExecutor::new(
        provider(),
        vec![],
        None,
        128,
        tx,
        temp.path().to_string_lossy().into_owned(),
        ReasoningMode::Standard,
    )
    .with_conv_id("tool-loading-fixture".into())
    .with_permission_manager(manager)
    .with_search_config_path(temp.path().join("config.json"));
    let (_, output, failed) = executor
        .execute_tool_call(
            "plan_mode",
            &json!({"operation":"create","task":"legacy","steps":[]}),
            "retired",
        )
        .await;
    assert!(failed, "{output}");
    assert!(output.contains("Unknown tool: plan_mode"), "{output}");
    assert!(
        rx.try_recv().is_err(),
        "a retired tool must not request permission"
    );
    assert!(!temp.path().join(".claude").exists());
}

/// The probe states from the second audit: a call from the wrong role, a result
/// placed before its call, and a repeated id must all stay unloaded.
#[test]
fn a_restored_tool_needs_an_assistant_call_before_its_own_result() {
    let temp = tempfile::tempdir().unwrap();
    let restore = |messages: Vec<Value>| {
        let mut executor = executor(temp.path());
        executor.messages = messages;
        executor.restore_loaded_tools();
        names(&executor)
    };
    let loaded = || json!({"success":true,"loaded_tools":["Monitor"],"tools":[]}).to_string();
    let call = |role: &str, id: &str| json!({"role":role,"content":[{"type":"tool_use","id":id,"name":"ToolLoad","input":{}}]});
    let result = |id: &str| json!({"role":"user","content":[{"type":"tool_result","tool_use_id":id,"content":loaded()}]});
    let granted = |messages: Vec<Value>| restore(messages).contains(&"Monitor".to_owned());

    // A tool_use on a user turn is not an assistant call.
    assert!(!granted(vec![call("user", "x1"), result("x1")]));
    // The result arrives before the call that would explain it.
    assert!(!granted(vec![result("x2"), call("assistant", "x2")]));
    // Two results for one call: only the first may count, so a later one cannot
    // turn an ignored answer into a load.
    assert!(!granted(vec![
        call("assistant", "x3"),
        json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"x3","content":"{\"success\":false,\"loaded_tools\":[]}"}]}),
        result("x3"),
    ]));
    // A result in an assistant turn is not a result.
    assert!(!granted(vec![
        call("assistant", "x4"),
        json!({"role":"assistant","content":[{"type":"tool_result","tool_use_id":"x4","content":loaded()}]})
    ]));
    // The ordinary shape still works.
    assert!(granted(vec![call("assistant", "x5"), result("x5")]));
}

/// The third audit's two probe states: a repeated call id must not take over
/// another call's identity, and a consumed id must not be registered again.
#[test]
fn a_reused_call_id_never_grants_a_tool() {
    let temp = tempfile::tempdir().unwrap();
    let granted = |messages: Vec<Value>| {
        let mut executor = executor(temp.path());
        executor.messages = messages;
        executor.restore_loaded_tools();
        names(&executor).contains(&"Monitor".to_owned())
    };
    let loaded = || json!({"success":true,"loaded_tools":["Monitor"],"tools":[]}).to_string();
    let openai_call = |id: &str, name: &str| json!({"role":"assistant","content":"","tool_calls":[{"id":id,"type":"function","function":{"name":name,"arguments":"{}"}}]});
    let openai_result =
        |id: &str, text: &str| json!({"role":"tool","tool_call_id":id,"content":text});
    let anth_call = |id: &str, name: &str| json!({"role":"assistant","content":[{"type":"tool_use","id":id,"name":name,"input":{}}]});
    let anth_result = |id: &str, text: &str, is_error: bool| json!({"role":"user","content":[{"type":"tool_result","tool_use_id":id,"content":text,"is_error":is_error}]});

    // A second call with the same id must not rename the first one's result.
    assert!(!granted(vec![
        openai_call("dup", "Read"),
        openai_call("dup", "ToolLoad"),
        openai_result("dup", &loaded()),
    ]));
    // The same on the Anthropic shape, within one assistant turn.
    assert!(!granted(vec![
        json!({"role":"assistant","content":[
            {"type":"tool_use","id":"dup2","name":"Read","input":{}},
            {"type":"tool_use","id":"dup2","name":"ToolLoad","input":{}}
        ]}),
        anth_result("dup2", &loaded(), false),
    ]));
    // A consumed id cannot be registered again to revive an old failure.
    assert!(!granted(vec![
        openai_call("once", "ToolLoad"),
        openai_result("once", "{\"success\":false,\"loaded_tools\":[]}"),
        openai_call("once", "ToolLoad"),
        openai_result("once", &loaded()),
    ]));
    assert!(!granted(vec![
        anth_call("once2", "ToolLoad"),
        anth_result("once2", "{\"success\":false,\"loaded_tools\":[]}", false),
        anth_call("once2", "ToolLoad"),
        anth_result("once2", &loaded(), false),
    ]));
    // A second result for one successful call still counts only once, and the
    // rule above must not break ordinary distinct ids in one turn.
    assert!(granted(vec![
        json!({"role":"assistant","content":[
            {"type":"tool_use","id":"ok1","name":"ToolLoad","input":{}},
            {"type":"tool_use","id":"ok2","name":"Read","input":{}}
        ]}),
        json!({"role":"user","content":[
            {"type":"tool_result","tool_use_id":"ok2","content":"file"},
            {"type":"tool_result","tool_use_id":"ok1","content":loaded()}
        ]}),
    ]));
    assert!(granted(vec![
        openai_call("turn1", "ToolLoad"),
        openai_result("turn1", &loaded()),
        openai_call("turn2", "Read"),
        openai_result("turn2", "file"),
    ]));
}

/// Turning a resident tool into an on-demand one must revoke its old exposure.
#[test]
fn a_resident_tool_switched_to_on_demand_needs_a_fresh_search() {
    let temp = tempfile::tempdir().unwrap();
    let mut executor = executor(temp.path());
    let exposed: std::collections::HashSet<String> = names(&executor).into_iter().collect();
    assert!(!exposed.contains("ListDir"));
    // Pretend the in-flight request carried ListDir as a resident core tool.
    let mut exposed = exposed;
    exposed.insert("ListDir".to_owned());
    executor.exposed_tools = Some(exposed);
    write_switches(temp.path(), &[("ListDir", true, true)]);
    assert!(executor.loading_violation("ListDir").is_none());

    // Now only discovery is left: the old request must not grant the call.
    write_switches(temp.path(), &[("ListDir", false, true)]);
    let violation = executor
        .loading_violation("ListDir")
        .expect("needs a search");
    assert!(violation.contains("not loaded"), "{violation}");

    // A tool that was actually searched stays callable in this request.
    executor.loaded_tools.insert("ListDir".to_owned());
    assert!(executor.loading_violation("ListDir").is_none());

    // Both off wins immediately over any earlier exposure or load.
    write_switches(temp.path(), &[("ListDir", false, false)]);
    let violation = executor.loading_violation("ListDir").expect("turned off");
    assert!(violation.contains("turned off"), "{violation}");

    // Turning discovery off while it stays resident does not break it.
    write_switches(temp.path(), &[("ListDir", true, false)]);
    assert!(executor.loading_violation("ListDir").is_none());
}

#[test]
fn an_executor_without_a_data_directory_uses_the_same_defaults() {
    let temp = tempfile::tempdir().unwrap();
    let mut executor = executor(temp.path());
    executor.search_config_path = None;
    let names = names(&executor);
    for name in ["Read", "Grep", "Glob", "ToolLoad"] {
        assert!(names.contains(&name.to_owned()), "{name}: {names:?}");
    }
    for name in ["ListDir", "Monitor", "computer_use"] {
        assert!(!names.contains(&name.to_owned()), "{name}: {names:?}");
    }
}

#[tokio::test]
async fn a_closed_tool_load_cannot_be_called_directly() {
    let temp = tempfile::tempdir().unwrap();
    write_switches(temp.path(), &[("ToolLoad", false, true)]);
    let mut executor = executor(temp.path());
    assert!(!names(&executor).contains(&"ToolLoad".to_owned()));
    let (_, output, failed) = executor
        .execute_tool_call("ToolLoad", &json!({"names":["ListDir"]}), "search")
        .await;
    assert!(failed, "{output}");
    assert!(output.contains("not loaded"), "{output}");
}

#[test]
fn a_tool_enabled_after_the_request_started_waits_for_the_next_one() {
    let temp = tempfile::tempdir().unwrap();
    let mut executor = executor(temp.path());
    let exposed: std::collections::HashSet<String> = names(&executor).into_iter().collect();
    executor.exposed_tools = Some(exposed);
    // The user enables ListDir while this response is still streaming.
    write_switches(temp.path(), &[("ListDir", true, true)]);
    let violation = executor
        .loading_violation("ListDir")
        .expect("not in this response");
    assert!(violation.contains("next model request"), "{violation}");
    // The next request does carry it.
    assert!(names(&executor).contains(&"ListDir".to_owned()));
}

#[tokio::test]
async fn tool_load_returns_the_description_override() {
    let temp = tempfile::tempdir().unwrap();
    let mut customization = crate::services::customization::Customization::default();
    customization.tool_descriptions.insert(
        "ListDir".to_owned(),
        "Find archived recordings in a folder".to_owned(),
    );
    crate::services::customization::save(temp.path(), &customization, &[]).unwrap();
    let mut executor = executor(temp.path());
    let (_, output, failed) = executor
        .execute_tool_call("ToolLoad", &json!({"names":["ListDir"]}), "search")
        .await;
    assert!(!failed, "{output}");
    let value: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(value["loaded_tools"], json!(["ListDir"]), "{output}");
    assert_eq!(
        value["tools"][0]["description"],
        "Find archived recordings in a folder"
    );
}

#[test]
fn the_loadable_reminder_shows_custom_text_uncut_and_the_whole_description_otherwise() {
    let temp = tempfile::tempdir().unwrap();
    let mut customization = crate::services::customization::Customization::default();
    let custom = format!(
        "Run directory listings for the project and {}.\nSecond line.",
        "keep every word ".repeat(10)
    );
    customization
        .tool_summaries
        .insert("ListDir".to_owned(), custom.clone());
    crate::services::customization::save(temp.path(), &customization, &[]).unwrap();
    let mut executor = executor(temp.path());
    let reminder = loadable_reminder(&executor).unwrap();
    let line = reminder.lines().find(|line| line.starts_with("- ListDir:")).unwrap();
    let one_line = custom.split_whitespace().collect::<Vec<_>>().join(" ");
    assert_eq!(line, format!("- ListDir: {one_line}"));
    // A tool without any custom text keeps its complete description.
    let full = format!("First sentence. {}\nSecond line.", "keep every word ".repeat(10));
    executor.mcp_states = std::collections::BTreeMap::from([("fixture".to_owned(), McpState::OnDemand)]);
    executor.mcp_tools = vec![crate::tools::ToolDefinition {
        summary: None,
        name: "mcp__fixture__long".into(),
        description: full.clone(),
        input_schema: json!({"type":"object"}),
    }];
    let reminder = loadable_reminder(&executor).unwrap();
    let line = reminder
        .lines()
        .find(|line| line.starts_with("- mcp__fixture__long:"))
        .unwrap();
    assert_eq!(
        line,
        format!("- mcp__fixture__long: {}", crate::prompt::one_line(&full))
    );
    assert!(line.ends_with("Second line."), "{line}");
    assert!(line.len() > 200, "the description is not cut: {line}");
}

#[test]
fn a_legacy_entry_outside_the_catalog_still_turns_that_tool_off() {
    let temp = tempfile::tempdir().unwrap();
    let customization = crate::services::customization::Customization {
        disabled_tools: vec!["mcp__old__echo".into()],
        ..Default::default()
    };
    crate::services::customization::save(temp.path(), &customization, &[]).unwrap();
    let mut executor = executor(temp.path());
    executor.mcp_tools = vec![crate::tools::ToolDefinition {
        summary: None,
        name: "mcp__old__echo".into(),
        description: "old".into(),
        input_schema: json!({"type":"object"}),
    }];
    assert!(!names(&executor).contains(&"mcp__old__echo".to_owned()));
    let violation = executor
        .loading_violation("mcp__old__echo")
        .expect("still off");
    assert!(violation.contains("turned off"), "{violation}");
    // An MCP tool the user did not disable keeps its existing path.
    executor.mcp_tools.push(crate::tools::ToolDefinition {
        summary: None,
        name: "mcp__live__echo".into(),
        description: "live".into(),
        input_schema: json!({"type":"object"}),
    });
    assert!(names(&executor).contains(&"mcp__live__echo".to_owned()));
    assert!(executor.loading_violation("mcp__live__echo").is_none());
}

#[tokio::test]
async fn tool_load_loads_a_tool_named_in_the_first_message_reminder() {
    let temp = tempfile::tempdir().unwrap();
    let mut executor = executor(temp.path());
    let reminder = loadable_reminder(&executor).expect("built-in tools are listed");
    // ListDir is on demand by default, so the reminder names it.
    assert!(reminder.contains("- ListDir:"), "{reminder}");
    for absent in ["# Tool directory", "MCP tools available on demand"] {
        assert!(!reminder.contains(absent), "{reminder}");
    }

    let (_, output, failed) = executor
        .execute_tool_call("ToolLoad", &json!({"names":["ListDir"]}), "from-reminder")
        .await;
    assert!(!failed, "{output}");
    let value: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(value["loaded_tools"], json!(["ListDir"]), "{output}");
    assert!(names(&executor).contains(&"ListDir".to_owned()));
}

/// Loading a tool appends it after the resident tools in load order, so the
/// cached request prefix never moves. Loading WebFetch then ListDir must not
/// fall back to the alphabetical ListDir, WebFetch.
#[tokio::test]
async fn loaded_tools_are_appended_after_the_resident_prefix_in_load_order() {
    let temp = tempfile::tempdir().unwrap();
    let mut executor = executor(temp.path());
    let resident = executor.tool_definitions().unwrap();
    let resident_names: Vec<String> = resident.iter().map(|tool| tool.name.clone()).collect();
    let resident_prefix = serde_json::to_string(&resident).unwrap();
    executor.exposed_tools = Some(resident_names.iter().cloned().collect());

    for (name, id) in [("Monitor", "load-mon"), ("ListDir", "load-dir")] {
        let (_, output, failed) = executor
            .execute_tool_call("ToolLoad", &json!({"names":[name]}), id)
            .await;
        assert!(!failed, "{output}");
    }

    let after = executor.tool_definitions().unwrap();
    let after_names: Vec<String> = after.iter().map(|tool| tool.name.clone()).collect();
    assert_eq!(
        &after_names[..resident_names.len()],
        resident_names.as_slice(),
        "the resident prefix must not move"
    );
    assert_eq!(
        &after_names[resident_names.len()..],
        ["Monitor", "ListDir"],
        "loaded tools follow the order they were loaded: {after_names:?}"
    );
    assert_eq!(
        serde_json::to_string(&after[..resident_names.len()]).unwrap(),
        resident_prefix,
        "the resident definitions stay byte-identical"
    );

    // Every later request rebuilds the same order.
    assert_eq!(names(&executor), after_names);
}

/// A user-provided tool has no loading switch and is resident even though its
/// name is not a built-in. Loading a builtin must not push it behind the
/// loaded definitions.
#[tokio::test]
async fn loading_a_tool_keeps_a_user_tool_in_the_resident_prefix() {
    use crate::services::customization::{CustomTool, Customization};
    let temp = tempfile::tempdir().unwrap();
    crate::services::customization::save(
        temp.path(),
        &Customization {
            custom_tools: vec![CustomTool {
                name: "zed_user_tool".into(),
                description: "A user tool.".into(),
                input_schema: json!({"type":"object","properties":{}}),
                command: "true".into(),
                enabled: true,
                timeout_secs: None,
            }],
            ..Default::default()
        },
        &[],
    )
    .unwrap();
    let mut executor = executor(temp.path());
    let resident = names(&executor);
    assert!(
        resident.contains(&"zed_user_tool".to_owned()),
        "{resident:?}"
    );
    executor.exposed_tools = Some(resident.iter().cloned().collect());
    let (_, output, failed) = executor
        .execute_tool_call("ToolLoad", &json!({"names":["Monitor"]}), "load")
        .await;
    assert!(!failed, "{output}");
    let after = names(&executor);
    assert_eq!(&after[..resident.len()], resident.as_slice(), "{after:?}");
    assert_eq!(after.last().map(String::as_str), Some("Monitor"), "{after:?}");
}

#[test]
fn resident_tool_order_is_stable_across_builds() {
    let temp = tempfile::tempdir().unwrap();
    let mut executor = executor(temp.path());
    executor.mcp_tools = vec![
        mcp_tool("mcp__z__second", "Second"),
        mcp_tool("mcp__a__first", "First"),
    ];
    executor.mcp_states = std::collections::BTreeMap::from([
        ("a".to_owned(), McpState::User),
        ("z".to_owned(), McpState::User),
    ]);

    let first = names(&executor);
    assert_eq!(names(&executor), first, "two builds of one session agree");
    let mut sorted = first.clone();
    sorted.sort();
    assert_eq!(first, sorted, "resident definitions are sorted by name: {first:?}");

    // A reconnecting registry that returns its tools in another order must not
    // rewrite the resident definitions.
    executor.mcp_tools.reverse();
    assert_eq!(names(&executor), first);
}

#[test]
fn mcp_descriptions_are_snapshotted_per_session() {
    let temp = tempfile::tempdir().unwrap();
    let mut session = executor(temp.path());
    session.mcp_tools = vec![mcp_tool("mcp__gh__echo", "Original description")];
    session.snapshot_mcp_tools().unwrap();

    session.mcp_tools = vec![mcp_tool("mcp__gh__echo", "Reconnected description")];
    session.snapshot_mcp_tools().unwrap();
    assert_eq!(
        session.mcp_tools[0].description, "Original description",
        "a reconnect cannot change a resident definition mid-session"
    );

    let mut fresh = executor(temp.path()).with_conv_id("a-new-session".into());
    fresh.mcp_tools = vec![mcp_tool("mcp__gh__echo", "Reconnected description")];
    fresh.snapshot_mcp_tools().unwrap();
    assert_eq!(
        fresh.mcp_tools[0].description, "Reconnected description",
        "a new session snapshots the current definition"
    );
}

#[test]
fn on_demand_mcp_tools_are_listed_with_builtins_first() {
    let temp = tempfile::tempdir().unwrap();
    let mut executor = executor(temp.path());
    let noisy = format!("{}\nA second line stays on the same line", "x".repeat(200));
    executor.mcp_tools = vec![
        mcp_tool(
            "mcp__gh__create_issue",
            "Create a new issue in a GitHub repository",
        ),
        mcp_tool("mcp__gh__noisy", &noisy),
        mcp_tool("mcp__fs__read_file", "Read a file"),
    ];
    executor.mcp_states = std::collections::BTreeMap::from([
        ("gh".to_owned(), McpState::OnDemand),
        ("fs".to_owned(), McpState::User),
    ]);

    let visible = names(&executor);
    assert!(visible.contains(&"mcp__fs__read_file".to_owned()), "{visible:?}");
    assert!(!visible.contains(&"mcp__gh__create_issue".to_owned()), "{visible:?}");
    assert!(!visible.contains(&"mcp__gh__noisy".to_owned()), "{visible:?}");

    let prompt = loadable_reminder(&executor).expect("on-demand tools are listed");
    assert!(
        prompt.starts_with(
            "<system-reminder>\nTools you can load with ToolLoad (pass exact names in the names array):\n"
        ),
        "{prompt}"
    );
    assert!(
        prompt.contains("- mcp__gh__create_issue: Create a new issue in a GitHub repository"),
        "{prompt}"
    );
    assert!(!prompt.contains("mcp__fs__read_file"), "{prompt}");
    // An MCP tool has no summary, so its whole description is listed on one line.
    let line = prompt
        .lines()
        .find(|line| line.starts_with("- mcp__gh__noisy:"))
        .expect("the second on-demand tool is listed");
    assert_eq!(line, format!("- mcp__gh__noisy: {} A second line stays on the same line", "x".repeat(200)), "{prompt}");
    // Built-ins come before the MCP block.
    let builtin = prompt.find("- ListDir:").unwrap();
    let mcp = prompt.find("- mcp__gh__create_issue:").unwrap();
    assert!(builtin < mcp, "{prompt}");

    executor.loaded_tools.insert("mcp__gh__create_issue".into());
    let request = executor.prepare_request().unwrap();
    assert!(request
        .tools
        .iter()
        .any(|tool| tool.name == "mcp__gh__create_issue"));
    // The first message is frozen: loading a tool never rewrites its list.
    let prompt = request.system_prompt.unwrap_or_default();
    assert!(!prompt.contains("mcp__gh__create_issue"), "{prompt}");
    let listing = loadable_reminder(&executor).unwrap();
    assert!(listing.contains("mcp__gh__create_issue"), "{listing}");
    assert!(listing.contains("mcp__gh__noisy"), "{listing}");
}

#[tokio::test]
async fn tool_load_loads_on_demand_mcp_tools() {
    let temp = tempfile::tempdir().unwrap();
    let mut executor = executor(temp.path());
    executor.mcp_tools = vec![
        mcp_tool(
            "mcp__gh__create_issue",
            "Create a new issue in a GitHub repository",
        ),
        mcp_tool(
            "mcp__gh__list_pull_requests",
            "List the open pull requests of a repository",
        ),
    ];
    executor.mcp_states =
        std::collections::BTreeMap::from([("gh".to_owned(), McpState::OnDemand)]);

    let (_, output, failed) = executor
        .execute_tool_call(
            "ToolLoad",
            &json!({"names":["mcp__gh__create_issue"]}),
            "search",
        )
        .await;
    assert!(!failed, "{output}");
    let value: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(
        value["loaded_tools"],
        json!(["mcp__gh__create_issue"]),
        "{output}"
    );
    assert!(executor.loaded_tools.contains("mcp__gh__create_issue"));
    // A resumed session restores the load from the stored ToolLoad result.
    let mut restored = LoadedTools::default();
    ToolLoopExecutor::absorb_loaded(&output, &mut restored);
    assert!(restored.contains("mcp__gh__create_issue"));

    assert!(names(&executor).contains(&"mcp__gh__create_issue".to_owned()));
    let prompt = executor.prepare_request().unwrap().system_prompt.unwrap_or_default();
    assert!(!prompt.contains("mcp__gh__create_issue"), "{prompt}");
    assert!(!prompt.contains("mcp__gh__list_pull_requests"), "{prompt}");
    let listing = loadable_reminder(&executor).unwrap();
    // The frozen first message keeps listing a tool the model loaded; only
    // the resident tool list changes.
    assert!(listing.contains("mcp__gh__create_issue"), "{listing}");
    assert!(listing.contains("mcp__gh__list_pull_requests"), "{listing}");

    // The other tool can be loaded by its exact qualified name.
    let (_, output, failed) = executor
        .execute_tool_call(
            "ToolLoad",
            &json!({"names":["mcp__gh__list_pull_requests"]}),
            "search",
        )
        .await;
    assert!(!failed, "{output}");
    let value: Value = serde_json::from_str(&output).unwrap();
    let loaded = value["loaded_tools"].as_array().unwrap();
    assert!(
        loaded.iter().any(|name| name == "mcp__gh__list_pull_requests"),
        "{output}"
    );
}

#[tokio::test]
async fn user_only_mcp_tools_are_hidden_and_refused() {
    let temp = tempfile::tempdir().unwrap();
    let mut executor = executor(temp.path());
    executor.mcp_tools = vec![mcp_tool(
        "mcp__gh__create_issue",
        "Create a new issue in a GitHub repository",
    )];
    executor.mcp_states =
        std::collections::BTreeMap::from([("gh".to_owned(), McpState::UserOnly)]);

    assert!(!names(&executor).contains(&"mcp__gh__create_issue".to_owned()));
    let prompt = executor.prepare_request().unwrap().system_prompt.unwrap_or_default();
    assert!(!prompt.contains("mcp__gh__create_issue"), "{prompt}");
    let listing = loadable_reminder(&executor).unwrap();
    assert!(!listing.contains("mcp__gh__create_issue"), "{listing}");

    let (_, output, failed) = executor
        .execute_tool_call(
            "ToolLoad",
            &json!({"names":["mcp__gh__create_issue"]}),
            "search",
        )
        .await;
    assert!(failed, "{output}");
    let value: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(value["success"], false, "{output}");
    assert_eq!(value["loaded_tools"], json!([]), "{output}");
    assert!(!executor.loaded_tools.contains("mcp__gh__create_issue"));


    let violation = executor
        .loading_violation("mcp__gh__create_issue")
        .expect("user-only is refused");
    assert!(violation.contains("user-only"), "{violation}");
}

#[test]
fn on_demand_mcp_tool_not_loaded_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let mut executor = executor(temp.path());
    executor.mcp_tools = vec![mcp_tool(
        "mcp__gh__create_issue",
        "Create a new issue in a GitHub repository",
    )];
    executor.mcp_states =
        std::collections::BTreeMap::from([("gh".to_owned(), McpState::OnDemand)]);

    let violation = executor
        .loading_violation("mcp__gh__create_issue")
        .expect("unloaded on-demand tool is refused");
    assert!(violation.contains("not loaded"), "{violation}");
    assert!(violation.contains("ToolLoad"), "{violation}");

    // A call in the same model response still has the old request's schemas.
    executor.exposed_tools = Some(names(&executor).into_iter().collect());
    executor.loaded_tools.insert("mcp__gh__create_issue".into());
    let violation = executor.loading_violation("mcp__gh__create_issue").unwrap();
    assert!(violation.contains("next model request"), "{violation}");

    executor.exposed_tools = Some(names(&executor).into_iter().collect());
    assert!(executor.loading_violation("mcp__gh__create_issue").is_none());
}

#[test]
fn a_restricted_agent_without_discovery_still_receives_its_granted_tools() {
    let temp = tempfile::tempdir().unwrap();
    let mut executor = executor(temp.path());
    executor.allowed_tools = Some(vec!["Read".into(), "ListDir".into()]);
    let visible = names(&executor);
    assert!(visible.contains(&"ListDir".to_owned()), "{visible:?}");
    assert!(visible.contains(&"Read".to_owned()), "{visible:?}");

    // With ToolLoad granted the agent keeps the normal on-demand rule.
    executor.allowed_tools = Some(vec!["Read".into(), "ListDir".into(), "ToolLoad".into()]);
    assert!(!names(&executor).contains(&"ListDir".to_owned()));
}

#[test]
fn damaged_settings_fail_the_request_instead_of_reopening_every_tool() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(temp.path().join("config")).unwrap();
    std::fs::write(temp.path().join("config/customization.json"), "{broken").unwrap();
    let executor = executor(temp.path());
    let error = executor.tool_definitions().unwrap_err().to_string();
    assert!(error.contains("customization.json"), "{error}");
}

fn wire_provider(base: &str, format: &str) -> ResolvedProvider {
    // `responses` uses the built-in OpenAI adapter with the Responses protocol,
    // like a Codex-style endpoint.
    if format == "responses" {
        return ResolvedProvider {
            provider: serde_json::from_value(json!({"id":"gateway","name":"gateway","builtin_id":"openai","base_url":format!("{base}/responses"),"api_key":"gateway-key","api_format":"openai","models":[],"enabled":true})).unwrap(),
            model: serde_json::from_value(json!({"id":"gpt-6-astra","name":"gpt-6-astra","api":"openai-responses","enabled":true,"supports_vision":false,"supports_web_search":false,"max_tokens":512})).unwrap(),
        };
    }
    let api_format = if format == "anthropic" {
        "anthropic"
    } else {
        "openai"
    };
    ResolvedProvider {
        provider: serde_json::from_value(json!({"id":"fixture","name":"fixture","base_url":base,"api_key":"unused","api_format":api_format,"models":[],"enabled":true})).unwrap(),
        model: serde_json::from_value(json!({"id":"code-test","name":"code-test","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap(),
    }
}

fn tool_call_events(format: &str, id: &str, name: &str, arguments: Value) -> Vec<Value> {
    let args = arguments.to_string();
    if format == "anthropic" {
        vec![
            json!({"type":"message_start","message":{"id":"test","model":"code-test","usage":{"input_tokens":20}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":id,"name":name,"input":{}}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":args}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":10}}),
            json!({"type":"message_stop"}),
        ]
    } else if format == "responses" {
        let item = json!({"type":"function_call","id":format!("fc-{id}"),"call_id":id,"name":name,"arguments":args});
        vec![
            json!({"type":"response.output_item.done","item":item}),
            json!({"type":"response.completed","response":{"id":"r","model":"code-test","status":"completed","output":[item],"usage":{"input_tokens":20,"output_tokens":10,"total_tokens":30}}}),
        ]
    } else {
        vec![
            json!({"id":"test","model":"code-test","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":name,"arguments":args}}]},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
        ]
    }
}

fn text_events(format: &str, text: &str) -> Vec<Value> {
    if format == "anthropic" {
        vec![
            json!({"type":"message_start","message":{"id":"test2","model":"code-test","usage":{"input_tokens":20}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),
            json!({"type":"content_block_stop","index":0}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":10}}),
            json!({"type":"message_stop"}),
        ]
    } else if format == "responses" {
        vec![
            json!({"type":"response.output_text.delta","delta":text}),
            json!({"type":"response.completed","response":{"id":"r2","model":"code-test","status":"completed","output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}],"usage":{"input_tokens":20,"output_tokens":10,"total_tokens":30}}}),
        ]
    } else {
        vec![
            json!({"id":"test2","model":"code-test","choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
        ]
    }
}

/// The system prompt a provider actually received, in each protocol's shape.
fn request_system_prompt(body: &Value, format: &str) -> String {
    match format {
        "openai" => body["messages"]
            .as_array()
            .and_then(|messages| messages.iter().find(|message| message["role"] == "system"))
            .and_then(|message| message["content"].as_str())
            .unwrap_or_default()
            .to_owned(),
        "responses" => body["instructions"].as_str().unwrap_or_default().to_owned(),
        _ => body["system"].as_str().unwrap_or_default().to_owned(),
    }
}

/// The first user message a provider actually received, serialized so each
/// protocol's text-block shape can be searched for the reminder.
fn request_first_user(body: &Value, format: &str) -> String {
    let items = if format == "responses" {
        body["input"].as_array()
    } else {
        body["messages"].as_array()
    };
    items
        .and_then(|messages| messages.iter().find(|message| message["role"] == "user"))
        .map(|message| message["content"].to_string())
        .unwrap_or_default()
}

fn write_directory_switches(root: &std::path::Path, websearch_on_demand: bool) {
    use crate::services::customization::{CustomTool, Customization, ToolLoading};
    let mut tool_loading = std::collections::BTreeMap::new();
    for (name, enabled, on_demand) in [
        ("WebFetch", true, false),
        ("computer_use", false, false),
        ("ListDir", false, true),
        ("WebSearch", false, websearch_on_demand),
    ] {
        tool_loading.insert(name.to_owned(), ToolLoading { enabled, on_demand });
    }
    let customization = Customization {
        tool_loading,
        tool_descriptions: [(
            "ListDir".to_owned(),
            "Directory listing from the shared snapshot.".to_owned(),
        )]
        .into_iter()
        .collect(),
        custom_tools: vec![CustomTool {
            name: "private_tool".into(),
            description: "A user tool supplied separately from the built-in catalog.".into(),
            input_schema: json!({"type":"object","properties":{}}),
            command: "true".into(),
            enabled: true,
            timeout_secs: None,
        }],
        ..Default::default()
    };
    crate::services::customization::save(root, &customization, &[]).unwrap();
}

fn request_tool_names(body: &Value, format: &str) -> Vec<String> {    body["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .map(|tool| {
                    let name = if format == "openai" {
                        &tool["function"]["name"]
                    } else {
                        &tool["name"]
                    };
                    name.as_str().unwrap_or_default().to_owned()
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The tool result texts a provider request actually carries.
fn request_tool_outputs(body: &Value, format: &str) -> Vec<String> {
    let text = |value: &Value| value.as_str().map(str::to_owned);
    match format {
        "openai" => body["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|message| message["role"] == "tool")
            .filter_map(|message| text(&message["content"]))
            .collect(),
        "responses" => body["input"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|item| item["type"] == "function_call_output")
            .filter_map(|item| text(&item["output"]))
            .collect(),
        _ => body["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|message| message["content"].as_array().into_iter().flatten())
            .filter(|block| block["type"] == "tool_result")
            .filter_map(|block| text(&block["content"]))
            .collect(),
    }
}

/// The tools in a sent body, normalized to the `{name, description,
/// input_schema}` shape the request snapshot stores.
fn canonical_tools(body: &Value, format: &str) -> Vec<Value> {
    body["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .map(|tool| match format {
                    "openai" => json!({
                        "name": tool["function"]["name"],
                        "description": tool["function"]["description"],
                        "input_schema": tool["function"]["parameters"],
                    }),
                    "responses" => json!({
                        "name": tool["name"],
                        "description": tool["description"],
                        "input_schema": tool["parameters"],
                    }),
                    _ => json!({
                        "name": tool["name"],
                        "description": tool["description"],
                        "input_schema": tool["input_schema"],
                    }),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A restored session's very first Responses request already carries the tool.
#[tokio::test]
async fn a_restored_search_reaches_the_first_responses_request() {
    use axum::{http::header, routing::post, Json, Router};
    let _ = crate::config::OrchestrationConfig::init_global();
    let root = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
    let requests = captured.clone();
    let app = Router::new().route(
        "/responses",
        post(move |Json(body): Json<Value>| {
            let requests = requests.clone();
            async move {
                requests.lock().await.push(body);
                let events = text_events("responses", "done");
                let stream = events
                    .into_iter()
                    .map(|event| {
                        format!(
                            "event: {}\ndata: {event}\n\n",
                            event["type"].as_str().unwrap_or_default()
                        )
                    })
                    .collect::<String>();
                ([(header::CONTENT_TYPE, "text/event-stream")], stream)
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let (tx, _rx) = mpsc::channel(64);
    // The stored history a resumed session would load: a real assistant search
    // and its matching successful result.
    let history = vec![
        json!({"role":"assistant","content":"","tool_calls":[{"id":"s1","type":"function","function":{"name":"ToolLoad","arguments":"{\"names\":[\"ListDir\"]}"}}]}),
        json!({"role":"tool","tool_call_id":"s1","content":json!({"success":true,"loaded_tools":["ListDir"],"tools":[]}).to_string()}),
        json!({"role":"user","content":"Continue"}),
    ];
    let mut executor = ToolLoopExecutor::new(
        wire_provider(&base, "responses"),
        history,
        None,
        256,
        tx,
        root.path().to_string_lossy().into_owned(),
        ReasoningMode::Standard,
    )
    .with_search_config_path(
        crate::data_layout::DataLayout::from_root(&root.path()).config_file("config.json"),
    );
    let (text, _) = tokio::time::timeout(Duration::from_secs(20), executor.execute())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(text, "done");
    server.abort();
    let requests = captured.lock().await;
    let names = request_tool_names(&requests[0], "responses");
    assert!(names.contains(&"ListDir".to_owned()), "{names:?}");
}

/// A settings change saved between the request and the response takes effect
/// for the call the model makes in that very response.
#[tokio::test]
async fn a_settings_change_during_a_turn_revokes_the_old_exposure() {
    use axum::{http::header, routing::post, Json, Router};
    let _ = crate::config::OrchestrationConfig::init_global();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("visible.txt"), "SHOULD_NOT_BE_LISTED").unwrap();
    // ListDir is resident for the first request.
    write_switches(root.path(), &[("ListDir", true, true)]);
    let marker = root.path().to_path_buf();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let seen = requests.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |Json(body): Json<Value>| {
            let seen = seen.clone();
            let dir = marker.clone();
            async move {
                let step = {
                    let mut seen = seen.lock().await;
                    seen.push(body);
                    seen.len() - 1
                };
                let events = match step {
                    0 => {
                        // The user saves "on demand only" while this response is
                        // still being produced.
                        write_switches(&dir, &[("ListDir", false, true)]);
                        tool_call_events("openai", "list-1", "ListDir", json!({"path":"."}))
                    }
                    _ => text_events("openai", "done"),
                };
                let mut stream = events
                    .into_iter()
                    .map(|event| format!("data: {event}\n\n"))
                    .collect::<String>();
                stream.push_str("data: [DONE]\n\n");
                ([(header::CONTENT_TYPE, "text/event-stream")], stream)
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let (tx, mut rx) = mpsc::channel(128);
    let mut executor = ToolLoopExecutor::new(
        wire_provider(&base, "openai"),
        vec![json!({"role":"user","content":"List the files"})],
        None,
        256,
        tx,
        root.path().to_string_lossy().into_owned(),
        ReasoningMode::Standard,
    )
    .with_search_config_path(
        crate::data_layout::DataLayout::from_root(&root.path()).config_file("config.json"),
    );
    let (text, _) = tokio::time::timeout(Duration::from_secs(20), executor.execute())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(text, "done");
    server.abort();

    let mut rejected = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let EngineEvent::ToolUseDone {
            tool_name,
            is_error,
            output,
            ..
        } = event
        {
            if is_error {
                rejected.push(format!("{tool_name}: {output}"));
            }
        }
    }
    assert_eq!(rejected.len(), 1, "{rejected:?}");
    assert!(rejected[0].contains("not loaded"), "{rejected:?}");
    // The call did not run, so no directory listing reached the model.
    let requests = requests.lock().await;
    assert!(request_tool_names(&requests[0], "openai").contains(&"ListDir".to_owned()));
    assert!(
        !requests[1].to_string().contains("visible.txt"),
        "{:?}",
        requests[1]
    );
    assert!(!request_tool_names(&requests[1], "openai").contains(&"ListDir".to_owned()));
}

/// The real request path on all three transports: the first request omits an
/// on-demand tool, the search adds it to the next one with its real schema, and
/// the call then runs.
#[tokio::test]
async fn every_transport_adds_loaded_tools_to_the_next_request() {
    use axum::{http::header, routing::post, Json, Router};
    let _ = crate::config::OrchestrationConfig::init_global();
    for format in ["anthropic", "openai", "responses"] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("needle.txt"), "inside").unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
        let requests = captured.clone();
        let paths = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen_paths = paths.clone();
        let path = match format {
            "anthropic" => "/v1/messages",
            "responses" => "/responses",
            _ => "/v1/chat/completions",
        };
        let app = Router::new().route(
            path,
            post(move |uri: axum::http::Uri, Json(body): Json<Value>| {
                let requests = requests.clone();
                let paths = seen_paths.clone();
                async move {
                    let mut requests = requests.lock().await;
                    let mut paths = paths.lock().await;
                    let step = requests.len();
                    requests.push(body);
                    paths.push(uri.path().to_owned());
                    drop(paths);
                    let events = match step {
                        0 => tool_call_events(
                            format,
                            "search-1",
                            "ToolLoad",
                            json!({"names":["ListDir","Monitor"]}),
                        ),
                        1 => tool_call_events(format, "list-1", "ListDir", json!({"path":"."})),
                        _ => text_events(format, "done"),
                    };
                    let mut stream = if format == "responses" {
                        // Responses streams name their event type.
                        events
                            .into_iter()
                            .map(|event| {
                                format!(
                                    "event: {}\ndata: {event}\n\n",
                                    event["type"].as_str().unwrap_or_default()
                                )
                            })
                            .collect::<String>()
                    } else {
                        events
                            .into_iter()
                            .map(|event| format!("data: {event}\n\n"))
                            .collect::<String>()
                    };
                    if format == "openai" {
                        stream.push_str("data: [DONE]\n\n");
                    }
                    ([(header::CONTENT_TYPE, "text/event-stream")], stream)
                }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let (tx, mut rx) = mpsc::channel(256);
        let mut executor = ToolLoopExecutor::new(
            wire_provider(&base, format),
            vec![json!({"role":"user","content":"Find the text files"})],
            None,
            512,
            tx,
            root.path().to_string_lossy().into_owned(),
            ReasoningMode::Standard,
        )
        .with_search_config_path(
            crate::data_layout::DataLayout::from_root(&root.path()).config_file("config.json"),
        );
        let (text, _) = tokio::time::timeout(Duration::from_secs(20), executor.execute())
            .await
            .unwrap()
            .unwrap();
        let mut errors = Vec::new();
        let mut tool_load_outputs = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let EngineEvent::ToolUseDone {
                tool_name,
                is_error,
                output,
                ..
            } = event
            {
                if is_error {
                    errors.push(format!("{tool_name}: {output}"));
                }
                if tool_name == "ToolLoad" {
                    tool_load_outputs.push(output);
                }
            }
        }
        assert!(errors.is_empty(), "{format}: {errors:?}");
        // Desktop and the SDK keep the structured result while the model sees
        // only its content line.
        assert_eq!(tool_load_outputs.len(), 1, "{format}");
        let structured: Value = serde_json::from_str(&tool_load_outputs[0]).unwrap();
        assert_eq!(
            structured["loaded_tools"],
            json!(["ListDir", "Monitor"]),
            "{format}"
        );
        assert!(
            structured["tools"][0]["input_schema"].is_object(),
            "{format}"
        );
        assert_eq!(text, "done", "{format}");
        server.abort();

        let requests = captured.lock().await;
        assert_eq!(requests.len(), 3, "{format}");
        assert_eq!(*paths.lock().await, vec![path.to_owned(); 3], "{format}");
        let first = request_tool_names(&requests[0], format);
        assert!(first.contains(&"Glob".to_owned()), "{format}: {first:?}");
        assert!(
            !first.contains(&"ListDir".to_owned()),
            "{format}: {first:?}"
        );
        let second = request_tool_names(&requests[1], format);
        assert!(
            second.contains(&"ListDir".to_owned()),
            "{format}: {second:?}"
        );
        assert!(
            second.contains(&"Monitor".to_owned()),
            "{format}: {second:?}"
        );
        // The resident tools keep their exact prefix, and the loaded tools
        // follow in the order the search named them.
        let first_tools = requests[0]["tools"].as_array().unwrap();
        let second_tools = requests[1]["tools"].as_array().unwrap();
        assert_eq!(
            &second_tools[..first_tools.len()],
            first_tools.as_slice(),
            "{format}: loading moved a resident tool"
        );
        assert_eq!(
            &second[first_tools.len()..],
            ["ListDir", "Monitor"],
            "{format}: {second:?}"
        );
        assert!(
            requests[1]["tools"].to_string().contains("\"path\""),
            "{format}"
        );
        // The model-visible result is only the content line: the next request
        // already carries both definitions in its tools array.
        let outputs = request_tool_outputs(&requests[1], format);
        assert_eq!(
            outputs,
            ["Loaded ListDir, Monitor. Available from the next request."],
            "{format}: {outputs:?}"
        );
        assert!(requests[2].to_string().contains("needle.txt"), "{format}");
    }
}

/// A settings change saved after the request was sent is evaluated against the
/// current switches for the call in that response.
#[tokio::test]
async fn a_settings_change_while_a_loaded_tool_is_called_follows_the_current_switches() {
    use axum::{http::header, routing::post, Json, Router};
    let _ = crate::config::OrchestrationConfig::init_global();
    for case in ["double_off", "enabled_without_on_demand", "damaged"] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("marker.txt"), "listed").unwrap();
        write_switches(root.path(), &[("ListDir", false, true)]);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
        let requests = captured.clone();
        let dir = root.path().to_path_buf();
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |Json(body): Json<Value>| {
                let requests = requests.clone();
                let dir = dir.clone();
                async move {
                    let step = {
                        let mut seen = requests.lock().await;
                        seen.push(body);
                        seen.len() - 1
                    };
                    let events = match step {
                        0 => tool_call_events(
                            "openai",
                            "load-1",
                            "ToolLoad",
                            json!({"names":["ListDir"]}),
                        ),
                        1 => {
                            match case {
                                "double_off" => write_switches(&dir, &[("ListDir", false, false)]),
                                "enabled_without_on_demand" => {
                                    write_switches(&dir, &[("ListDir", true, false)])
                                }
                                _ => {
                                    std::fs::create_dir_all(dir.join("config")).unwrap();
                                    std::fs::write(dir.join("config/customization.json"), b"{")
                                        .unwrap()
                                }
                            }
                            tool_call_events("openai", "list-1", "ListDir", json!({"path":"."}))
                        }
                        _ => text_events("openai", "done"),
                    };
                    (
                        [(header::CONTENT_TYPE, "text/event-stream")],
                        sse_stream("openai", events),
                    )
                }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let (tx, mut rx) = mpsc::channel(256);
        let mut executor = ToolLoopExecutor::new(
            wire_provider(&base, "openai"),
            vec![json!({"role":"user","content":"List the files"})],
            None,
            512,
            tx,
            root.path().to_string_lossy().into_owned(),
            ReasoningMode::Standard,
        )
        .with_search_config_path(
            crate::data_layout::DataLayout::from_root(&root.path()).config_file("config.json"),
        )
        .with_permission_manager({
            let manager = crate::runtime::default_permissions();
            manager.set_mode(crate::permissions::PermissionMode::BypassPermissions);
            manager
        });
        let result = timeout(Duration::from_secs(30), executor.execute())
            .await
            .unwrap_or_else(|_| panic!("{case}: the turn timed out"));
        server.abort();

        let mut rejected = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let EngineEvent::ToolUseDone {
                tool_name,
                is_error,
                output,
                ..
            } = event
            {
                if is_error {
                    rejected.push(format!("{tool_name}: {output}"));
                }
            }
        }
        let requests = captured.lock().await;
        assert!(!request_tool_names(&requests[0], "openai").contains(&"ListDir".to_owned()));
        assert!(request_tool_names(&requests[1], "openai").contains(&"ListDir".to_owned()));
        match case {
            "double_off" => {
                result.unwrap();
                assert_eq!(rejected.len(), 1, "{rejected:?}");
                assert!(rejected[0].contains("turned off"), "{rejected:?}");
                assert_eq!(requests.len(), 3, "the turn must reach its answer");
                assert!(
                    !requests[2].to_string().contains("marker.txt"),
                    "the rejected call must not run"
                );
                assert!(
                    !request_tool_names(&requests[2], "openai").contains(&"ListDir".to_owned()),
                    "the next request must drop the tool"
                );
            }
            "enabled_without_on_demand" => {
                result.unwrap();
                assert!(rejected.is_empty(), "{rejected:?}");
                assert_eq!(requests.len(), 3);
                assert!(
                    requests[2].to_string().contains("marker.txt"),
                    "an enabled tool keeps running"
                );
                assert!(request_tool_names(&requests[2], "openai").contains(&"ListDir".to_owned()));
            }
            _ => {
                assert!(result.is_err(), "a damaged file must fail the request");
                assert_eq!(rejected.len(), 1, "{rejected:?}");
                assert!(rejected[0].contains("could not be read"), "{rejected:?}");
                assert_eq!(
                    requests.len(),
                    2,
                    "the next request must not be sent with damaged settings"
                );
            }
        }
    }
}

/// The real request payloads on all three transports keep the loadable-tool
/// list out of the system prompt. The frozen first-message reminder is built
/// from the initial switches; resident, disabled, custom and MCP tools are not
/// listed, and loading or disabling a tool never rewrites that prefix.
#[tokio::test]
async fn every_transport_moves_the_loadable_tool_list_to_the_first_message() {
    use axum::{http::header, routing::post, Json, Router};
    let _ = crate::config::OrchestrationConfig::init_global();
    for format in ["anthropic", "openai", "responses"] {
        let root = tempfile::tempdir().unwrap();
        write_directory_switches(root.path(), true);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
        let requests = captured.clone();
        let dir = root.path().to_path_buf();
        let app = Router::new().route(
            request_path(format),
            post(move |Json(body): Json<Value>| {
                let requests = requests.clone();
                let dir = dir.clone();
                async move {
                    let step = {
                        let mut seen = requests.lock().await;
                        seen.push(body);
                        seen.len() - 1
                    };
                    let events = match step {
                        0 => {
                            // The user disables WebSearch while the search result
                            // is being produced, before the next request is built.
                            write_directory_switches(&dir, false);
                            tool_call_events(
                                format,
                                "search-1",
                                "ToolLoad",
                                json!({"names":["ListDir"]}),
                            )
                        }
                        _ => text_events(format, "done"),
                    };
                    (
                        [(header::CONTENT_TYPE, "text/event-stream")],
                        sse_stream(format, events),
                    )
                }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let (tx, _rx) = mpsc::channel(64);
        let mut executor = ToolLoopExecutor::new(
            wire_provider(&base, format),
            vec![json!({"role":"user","content":"Find the files"})],
            Some("BASE SYSTEM".into()),
            512,
            tx,
            root.path().to_string_lossy().into_owned(),
            ReasoningMode::Standard,
        )
        .with_search_config_path(
            crate::data_layout::DataLayout::from_root(&root.path()).config_file("config.json"),
        );
        let reminder = loadable_reminder(&executor).expect("the initial switches list tools");
        executor = executor.with_context_reminders(vec![reminder.clone()]);
        let (text, _) = tokio::time::timeout(Duration::from_secs(20), executor.execute())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(text, "done", "{format}");
        server.abort();
        let requests = captured.lock().await;
        assert_eq!(requests.len(), 2, "{format}");

        let first = request_system_prompt(&requests[0], format);
        assert!(first.contains("BASE SYSTEM"), "{format}: {first}");
        assert!(!first.contains("# Tool directory"), "{format}: {first}");
        assert!(!first.contains("Tools you can load with ToolLoad"), "{format}: {first}");
        assert!(!first.contains("ListDir"), "{format}: {first}");
        assert!(!first.contains("MCP tools available on demand"), "{format}: {first}");

        let user = request_first_user(&requests[0], format);
        assert!(user.contains("Tools you can load with ToolLoad"), "{format}: {user}");
        assert!(
            user.contains("- ListDir: Directory listing from the shared snapshot"),
            "{format}: the listing uses the configured override: {user}"
        );
        assert!(user.contains("- WebSearch:"), "{format}: {user}");
        assert!(user.contains("- Monitor:"), "{format}: {user}");
        for absent in [
            "- Bash",
            "- WebFetch",
            "- MultiEdit",
            "- TaskGet",
            "computer_use",
            "private_tool",
            "git_status",
            "git_diff",
        ] {
            assert!(!user.contains(absent), "{format}: {absent} in {user}");
        }
        assert!(!request_tool_names(&requests[0], format).contains(&"ListDir".to_owned()));

        // The first message stays exactly as frozen, even though ListDir was
        // loaded and WebSearch was disabled between the two requests.
        let frozen_user = request_first_user(&requests[1], format);
        assert!(frozen_user.contains("- ListDir: Directory listing from the shared snapshot"), "{format}: {frozen_user}");
        assert!(frozen_user.contains("- WebSearch:"), "{format}: {frozen_user}");
        assert!(frozen_user.contains("- Monitor:"), "{format}: {frozen_user}");
        assert_eq!(
            frozen_user.matches("Tools you can load with ToolLoad").count(),
            1,
            "{format}: {frozen_user}"
        );
        let second = request_system_prompt(&requests[1], format);
        assert!(second.contains("BASE SYSTEM"), "{format}: {second}");
        assert!(!second.contains("ListDir"), "{format}: {second}");
        assert!(!second.contains("WebSearch"), "{format}: {second}");
        assert!(request_tool_names(&requests[1], format).contains(&"ListDir".to_owned()));
        // The loaded schema carries the same override the listing used.
        assert!(
            requests[1]["tools"]
                .to_string()
                .contains("Directory listing from the shared snapshot"),
            "{format}: {second}"
        );
    }
}

/// Mock transport requests carry the same MCP eligibility as ToolLoad.
#[tokio::test]
async fn every_transport_lists_only_loadable_mcp_tools_when_the_loader_is_present() {
    use axum::{http::header, routing::post, Json, Router};
    let _ = crate::config::OrchestrationConfig::init_global();
    let eligible = "mcp__fixture__eligible";
    let disabled = "mcp__fixture__disabled";
    let denied = "mcp__fixture__denied";
    for format in ["anthropic", "openai", "responses"] {
        for loader_enabled in [true, false] {
            let root = tempfile::tempdir().unwrap();
            crate::data_layout::write_config_file(
                crate::data_layout::DataLayout::from_root(&root.path())
                    .config_file("customization.json"),
                json!({
                    "disabled_tools": [disabled],
                    "tool_loading": {"ToolLoad": {"enabled": loader_enabled, "on_demand": false}}
                })
                .to_string(),
            )
            .unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
            let requests = captured.clone();
            let app = Router::new().route(
                request_path(format),
                post(move |Json(body): Json<Value>| {
                    let requests = requests.clone();
                    async move {
                        let mut seen = requests.lock().await;
                        let first = seen.is_empty();
                        seen.push(body);
                        let events = if first && loader_enabled {
                            tool_call_events(
                                format,
                                "load-mcp",
                                "ToolLoad",
                                json!({"names":[eligible]}),
                            )
                        } else {
                            text_events(format, "done")
                        };
                        (
                            [(header::CONTENT_TYPE, "text/event-stream")],
                            sse_stream(format, events),
                        )
                    }
                }),
            );
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let (tx, _rx) = mpsc::channel(64);
            let mut executor = ToolLoopExecutor::new(
                wire_provider(&base, format),
                vec![json!({"role":"user","content":"Load an on-demand tool"})],
                Some("BASE SYSTEM".into()),
                512,
                tx,
                root.path().to_string_lossy().into_owned(),
                ReasoningMode::Standard,
            )
            .with_search_config_path(
                crate::data_layout::DataLayout::from_root(&root.path()).config_file("config.json"),
            )
            .with_allowed_tools(Some(vec![
                "ToolLoad".into(),
                eligible.into(),
                disabled.into(),
            ]));
            // Inject a discovered catalog and drive the actual model transport.
            executor.mcp_tools = [eligible, disabled, denied]
                .into_iter()
                .map(|name| mcp_tool(name, "Fixture description"))
                .collect();
            executor
                .mcp_states
                .insert("fixture".into(), McpState::OnDemand);
            // The reminder the first user message would carry. Without ToolLoad
            // there is nothing a model could load, so the engine sends none.
            let reminder = loadable_reminder(&executor);
            if loader_enabled {
                executor = executor.with_context_reminders(vec![reminder
                    .clone()
                    .expect("the eligible MCP tool is listed")]);
            } else {
                assert!(reminder.is_none(), "{format}: {reminder:?}");
            }
            let (text, _) = timeout(Duration::from_secs(20), async {
                if format == "anthropic" {
                    executor.execute_anthropic_loop().await
                } else {
                    executor.execute_openai_loop().await
                }
            })
            .await
            .unwrap()
            .unwrap();
            assert_eq!(text, "done", "{format}");
            server.abort();
            let requests = captured.lock().await;
            assert_eq!(
                requests.len(),
                if loader_enabled { 2 } else { 1 },
                "{format}"
            );
            for body in requests.iter() {
                let prompt = request_system_prompt(body, format);
                let tools = request_tool_names(body, format);
                assert!(prompt.contains("BASE SYSTEM"), "{format}: {prompt}");
                assert!(
                    !prompt.contains("Tools you can load with ToolLoad")
                        && !prompt.contains("MCP tools available on demand"),
                    "{format}: the list must stay out of the system prompt: {prompt}"
                );
                assert_eq!(
                    tools.iter().any(|name| name == "ToolLoad"),
                    loader_enabled,
                    "{format}"
                );
                for name in [disabled, denied] {
                    assert!(!prompt.contains(name), "{format}: {prompt}");
                    assert!(
                        !tools.iter().any(|tool| tool == name),
                        "{format}: {tools:?}"
                    );
                }
            }
            let first = request_first_user(&requests[0], format);
            assert_eq!(
                first.contains(eligible),
                loader_enabled,
                "{format}: the first message carries the loadable MCP tool: {first}"
            );
            assert!(
                !request_tool_names(&requests[0], format)
                    .iter()
                    .any(|name| name == eligible),
                "{format}"
            );
            if loader_enabled {
                assert!(
                    request_tool_names(&requests[1], format)
                        .iter()
                        .any(|name| name == eligible),
                    "{format}"
                );
                // The first message is frozen; loading the tool never edits it.
                assert!(
                    request_first_user(&requests[1], format).contains(eligible),
                    "{format}: {}",
                    request_first_user(&requests[1], format)
                );
            }
        }
    }
}

/// An empty searchable set produces no reminder at all rather than a
/// misleading empty or loader-less list.
#[tokio::test]
async fn an_empty_loadable_set_produces_no_reminder() {
    use axum::{http::header, routing::post, Json, Router};
    let _ = crate::config::OrchestrationConfig::init_global();
    for allowed in [vec!["Bash", "ToolLoad"], vec!["Bash"]] {
        let root = tempfile::tempdir().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
        let requests = captured.clone();
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |Json(body): Json<Value>| {
                let requests = requests.clone();
                async move {
                    requests.lock().await.push(body);
                    (
                        [(header::CONTENT_TYPE, "text/event-stream")],
                        sse_stream("openai", text_events("openai", "done")),
                    )
                }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let (tx, _rx) = mpsc::channel(64);
        let executor = ToolLoopExecutor::new(
            wire_provider(&base, "openai"),
            vec![json!({"role":"user","content":"hello"})],
            Some("BASE SYSTEM".into()),
            512,
            tx,
            root.path().to_string_lossy().into_owned(),
            ReasoningMode::Standard,
        )
        .with_search_config_path(
            crate::data_layout::DataLayout::from_root(&root.path()).config_file("config.json"),
        )
        .with_allowed_tools(Some(
            allowed.iter().map(|name| (*name).to_owned()).collect(),
        ));
        assert!(
            loadable_reminder(&executor).is_none(),
            "{allowed:?} has nothing ToolLoad can load"
        );
        let mut executor = executor;
        let (text, _) = tokio::time::timeout(Duration::from_secs(20), executor.execute())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(text, "done");
        server.abort();
        let requests = captured.lock().await;
        let system = request_system_prompt(&requests[0], "openai");
        assert!(!system.contains("Tools you can load with ToolLoad"), "{system}");
        assert!(!system.contains("# Tool directory"), "{system}");
    }
}

#[tokio::test]
async fn a_request_without_callable_tool_load_does_not_promise_loading() {
    use axum::{http::header, routing::post, Json, Router};
    let _ = crate::config::OrchestrationConfig::init_global();
    for (label, enabled, on_demand) in [("both off", false, false), ("on demand only", false, true)] {
        let root = tempfile::tempdir().unwrap();
        write_switches(root.path(), &[("ToolLoad", enabled, on_demand)]);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
        let requests = captured.clone();
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |Json(body): Json<Value>| {
                let requests = requests.clone();
                async move {
                    requests.lock().await.push(body);
                    (
                        [(header::CONTENT_TYPE, "text/event-stream")],
                        sse_stream("openai", text_events("openai", "done")),
                    )
                }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let (tx, _rx) = mpsc::channel(64);
        let mut executor = ToolLoopExecutor::new(
            wire_provider(&base, "openai"),
            vec![json!({"role":"user","content":"hello"})],
            None,
            512,
            tx,
            root.path().to_string_lossy().into_owned(),
            ReasoningMode::Standard,
        )
        .with_search_config_path(
            crate::data_layout::DataLayout::from_root(&root.path()).config_file("config.json"),
        );
        let (text, _) = tokio::time::timeout(Duration::from_secs(20), executor.execute())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(text, "done", "{label}");
        server.abort();
        let requests = captured.lock().await;
        let sent = request_tool_names(&requests[0], "openai");
        assert!(!sent.contains(&"ToolLoad".to_owned()), "{label}: {sent:?}");
        assert!(sent.contains(&"Bash".to_owned()), "{label}: {sent:?}");
        let system = request_system_prompt(&requests[0], "openai");
        assert!(
            !system.contains("Tools you can load with ToolLoad"),
            "{label}: {system}"
        );
        assert!(!system.contains("ToolLoad is not available"), "{label}: {system}");
    }
}

/// Encodes provider events the way a real endpoint streams them.
fn sse_stream(format: &str, events: Vec<Value>) -> String {
    let mut stream = if format == "responses" {
        events
            .into_iter()
            .map(|event| {
                format!(
                    "event: {}\ndata: {event}\n\n",
                    event["type"].as_str().unwrap_or_default()
                )
            })
            .collect::<String>()
    } else {
        events
            .into_iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect::<String>()
    };
    if format == "openai" {
        stream.push_str("data: [DONE]\n\n");
    }
    stream
}

fn request_path(format: &str) -> &'static str {
    match format {
        "anthropic" => "/v1/messages",
        "responses" => "/responses",
        _ => "/v1/chat/completions",
    }
}

/// Loading a tool never bypasses the permission layer: the approval prompt is
/// still raised, and a denial leaves no side effect while the result the model
/// sees states the denial.
#[tokio::test]
async fn a_loaded_tool_still_asks_for_approval_and_a_denial_leaves_no_trace() {
    use axum::{http::header, routing::post, Json, Router};
    let _ = crate::config::OrchestrationConfig::init_global();
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("blocked.txt");
    write_switches(root.path(), &[("Write", false, true)]);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
    let requests = captured.clone();
    let path = target.to_string_lossy().into_owned();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |Json(body): Json<Value>| {
            let requests = requests.clone();
            let path = path.clone();
            async move {
                let step = {
                    let mut seen = requests.lock().await;
                    seen.push(body);
                    seen.len() - 1
                };
                let events = match step {
                    0 => tool_call_events(
                        "openai",
                        "search-1",
                        "ToolLoad",
                        json!({"names":["Write"]}),
                    ),
                    1 => tool_call_events(
                        "openai",
                        "write-1",
                        "Write",
                        json!({"file_path":path,"content":"must not be written"}),
                    ),
                    _ => text_events("openai", "done"),
                };
                (
                    [(header::CONTENT_TYPE, "text/event-stream")],
                    sse_stream("openai", events),
                )
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let manager = crate::runtime::default_permissions();
    manager.set_mode(crate::permissions::PermissionMode::AskPermissions);
    let (tx, mut rx) = mpsc::channel(256);
    let mut executor = ToolLoopExecutor::new(
        wire_provider(&base, "openai"),
        vec![json!({"role":"user","content":"Write the file"})],
        None,
        512,
        tx,
        root.path().to_string_lossy().into_owned(),
        ReasoningMode::Standard,
    )
    .with_search_config_path(
        crate::data_layout::DataLayout::from_root(&root.path()).config_file("config.json"),
    )
    .with_permission_manager(manager);
    executor.permission_host_available = true;
    let mailbox = executor.permission_mailbox.clone();
    let task = tokio::spawn(async move { executor.execute().await });
    let request_id = timeout(Duration::from_secs(15), async {
        loop {
            match rx.recv().await {
                Some(EngineEvent::ToolPermission {
                    request_id,
                    tool_name,
                    ..
                }) => {
                    assert_eq!(tool_name, "Write");
                    break request_id;
                }
                Some(_) => {}
                None => panic!("events ended before the approval request"),
            }
        }
    })
    .await
    .unwrap();
    crate::native_engine::permission_response::respond(
        &mailbox,
        &request_id,
        crate::native_engine::permission_response::PermissionResponse::Deny {
            message: "do not write".into(),
            interrupt: false,
        },
    )
    .await
    .unwrap();
    let (text, _) = timeout(Duration::from_secs(30), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(text, "done");
    server.abort();
    assert!(!target.exists(), "the denied write must not create the file");
    let mut denied = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let EngineEvent::ToolUseDone {
            tool_name,
            is_error,
            output,
            ..
        } = event
        {
            if is_error {
                denied.push(format!("{tool_name}: {output}"));
            }
        }
    }
    assert_eq!(denied.len(), 1, "{denied:?}");
    assert!(denied[0].contains("do not write"), "{denied:?}");
    let requests = captured.lock().await;
    assert_eq!(requests.len(), 3);
    assert!(request_tool_names(&requests[1], "openai").contains(&"Write".to_owned()));
    assert!(
        requests[2].to_string().contains("do not write"),
        "{:?}",
        requests[2]
    );
}

/// Cancelling while the approval prompt is open stops the turn: nothing runs
/// and no further model request is made.
#[tokio::test]
async fn cancelling_an_approval_stops_the_turn_without_running_the_tool() {
    use axum::{http::header, routing::post, Json, Router};
    let _ = crate::config::OrchestrationConfig::init_global();
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("blocked.txt");
    write_switches(root.path(), &[("Write", false, true)]);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
    let requests = captured.clone();
    let path = target.to_string_lossy().into_owned();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |Json(body): Json<Value>| {
            let requests = requests.clone();
            let path = path.clone();
            async move {
                let step = {
                    let mut seen = requests.lock().await;
                    seen.push(body);
                    seen.len() - 1
                };
                let events = match step {
                    0 => tool_call_events(
                        "openai",
                        "search-1",
                        "ToolLoad",
                        json!({"names":["Write"]}),
                    ),
                    1 => tool_call_events(
                        "openai",
                        "write-1",
                        "Write",
                        json!({"file_path":path,"content":"must not be written"}),
                    ),
                    _ => text_events("openai", "done"),
                };
                (
                    [(header::CONTENT_TYPE, "text/event-stream")],
                    sse_stream("openai", events),
                )
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let manager = crate::runtime::default_permissions();
    manager.set_mode(crate::permissions::PermissionMode::AskPermissions);
    let (tx, mut rx) = mpsc::channel(256);
    let mut executor = ToolLoopExecutor::new(
        wire_provider(&base, "openai"),
        vec![json!({"role":"user","content":"Write the file"})],
        None,
        512,
        tx,
        root.path().to_string_lossy().into_owned(),
        ReasoningMode::Standard,
    )
    .with_search_config_path(
        crate::data_layout::DataLayout::from_root(&root.path()).config_file("config.json"),
    )
    .with_permission_manager(manager);
    executor.permission_host_available = true;
    let task = tokio::spawn(async move { executor.execute().await });
    timeout(Duration::from_secs(15), async {
        loop {
            match rx.recv().await {
                Some(EngineEvent::ToolPermission { .. }) => break,
                Some(_) => {}
                None => panic!("events ended before the approval request"),
            }
        }
    })
    .await
    .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    tokio::time::sleep(Duration::from_millis(200)).await;
    server.abort();
    assert!(!target.exists(), "a cancelled approval must not run the tool");
    assert_eq!(
        captured.lock().await.len(),
        2,
        "the turn must not continue after cancellation"
    );
}

/// A running loaded tool is terminated when the turn is cancelled, and the
/// model loop does not start another request.
#[tokio::test]
async fn cancelling_a_running_loaded_tool_terminates_it_and_stops_the_turn() {
    use axum::{http::header, routing::post, Json, Router};
    let _ = crate::config::OrchestrationConfig::init_global();
    let root = tempfile::tempdir().unwrap();
    write_switches(root.path(), &[("Bash", false, true)]);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
    let requests = captured.clone();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |Json(body): Json<Value>| {
            let requests = requests.clone();
            async move {
                let step = {
                    let mut seen = requests.lock().await;
                    seen.push(body);
                    seen.len() - 1
                };
                let events = match step {
                    0 => tool_call_events(
                        "openai",
                        "search-1",
                        "ToolLoad",
                        json!({"names":["Bash"]}),
                    ),
                    1 => tool_call_events(
                        "openai",
                        "bash-1",
                        "Bash",
                        json!({"command":"echo ready > ready.txt; sleep 2; echo leaked > leaked.txt"}),
                    ),
                    _ => text_events("openai", "done"),
                };
                (
                    [(header::CONTENT_TYPE, "text/event-stream")],
                    sse_stream("openai", events),
                )
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let (tx, _rx) = mpsc::channel(256);
    let mut executor = ToolLoopExecutor::new(
        wire_provider(&base, "openai"),
        vec![json!({"role":"user","content":"Run the fixture command"})],
        None,
        512,
        tx,
        root.path().to_string_lossy().into_owned(),
        ReasoningMode::Standard,
    )
    .with_search_config_path(
        crate::data_layout::DataLayout::from_root(&root.path()).config_file("config.json"),
    )
    .with_permission_manager({
        let manager = crate::runtime::default_permissions();
        manager.set_mode(crate::permissions::PermissionMode::BypassPermissions);
        manager
    });
    let ready = root.path().join("ready.txt");
    let leaked = root.path().join("leaked.txt");
    let task = tokio::spawn(async move { executor.execute().await });
    timeout(Duration::from_secs(15), async {
        while !ready.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    tokio::time::sleep(Duration::from_secs(3)).await;
    server.abort();
    assert!(
        !leaked.exists(),
        "the cancelled shell command kept running in the background"
    );
    assert_eq!(
        captured.lock().await.len(),
        2,
        "the model loop must not continue after cancellation"
    );
}

/// Every HTTP attempt of one logical request carries the same tools list, and
/// the request snapshot and the token estimate describe that same list. Only
/// the next logical request reflects a tool loaded in between.
#[tokio::test]
async fn every_attempt_of_a_request_shares_one_tool_schema_and_estimation() {
    use axum::{
        http::{header, StatusCode},
        response::IntoResponse,
        routing::post,
        Json, Router,
    };
    let _ = crate::config::OrchestrationConfig::init_global();
    for format in ["anthropic", "openai", "responses"] {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("needle.txt"), "inside").unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
        let requests = captured.clone();
        let path = request_path(format);
        let app = Router::new().route(
            path,
            post(move |Json(body): Json<Value>| {
                let requests = requests.clone();
                async move {
                    let step = {
                        let mut seen = requests.lock().await;
                        seen.push(body);
                        seen.len() - 1
                    };
                    match step {
                        // The first HTTP attempt of the second logical request
                        // fails transiently; the retry must send the same body.
                        1 => (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({"error":{"message":"try again"}})),
                        )
                            .into_response(),
                        _ => {
                            let events = match step {
                                0 => tool_call_events(
                                    format,
                                    "search-1",
                                    "ToolLoad",
                                    json!({"names":["ListDir"]}),
                                ),
                                2 => tool_call_events(
                                    format,
                                    "list-1",
                                    "ListDir",
                                    json!({"path":"."}),
                                ),
                                _ => text_events(format, "done"),
                            };
                            (
                                [(header::CONTENT_TYPE, "text/event-stream")],
                                sse_stream(format, events),
                            )
                                .into_response()
                        }
                    }
                }
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let (tx, mut rx) = mpsc::channel(512);
        let mut executor = ToolLoopExecutor::new(
            wire_provider(&base, format),
            vec![json!({"role":"user","content":"Find the text files"})],
            None,
            512,
            tx,
            root.path().to_string_lossy().into_owned(),
            ReasoningMode::Standard,
        )
        .with_search_config_path(
            crate::data_layout::DataLayout::from_root(&root.path()).config_file("config.json"),
        );
        let (text, _) = timeout(Duration::from_secs(40), executor.execute())
            .await
            .unwrap_or_else(|_| panic!("{format}: the retried request timed out"))
            .unwrap();
        assert_eq!(text, "done", "{format}");
        server.abort();

        let mut snapshots = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let EngineEvent::RequestSnapshot(snapshot) = event {
                snapshots.push(snapshot);
            }
        }
        let requests = captured.lock().await;
        assert_eq!(requests.len(), 4, "{format}: {requests:#?}");
        assert_eq!(
            requests[1]["tools"], requests[2]["tools"],
            "{format}: the retry changed the tool schema"
        );
        let first = request_tool_names(&requests[0], format);
        let second = request_tool_names(&requests[1], format);
        let third = request_tool_names(&requests[3], format);
        assert!(!first.contains(&"ListDir".to_owned()), "{format}: {first:?}");
        assert!(second.contains(&"ListDir".to_owned()), "{format}: {second:?}");
        assert!(third.contains(&"ListDir".to_owned()), "{format}: {third:?}");
        assert!(
            requests[3].to_string().contains("needle.txt"),
            "{format}: the loaded tool did not run"
        );
        // The snapshot has to carry the exact descriptions and parameter
        // schemas the server received, not merely matching names.
        let sent_tools = canonical_tools(&requests[1], format);
        assert_eq!(
            sent_tools,
            canonical_tools(&requests[2], format),
            "{format}: the retry changed the schema contents"
        );

        let with_context: Vec<&Value> = snapshots
            .iter()
            .filter(|snapshot| !snapshot["contextUsage"].is_null())
            .collect();
        assert!(!with_context.is_empty(), "{format}: no context estimate");
        for snapshot in &with_context {
            assert!(
                snapshot["contextUsage"]["tools"]["estimated_tokens"]
                    .as_u64()
                    .unwrap_or_default()
                    > 0,
                "{format}: {snapshot}"
            );
            assert!(
                snapshot["contextUsage"]["estimated_input_tokens"]
                    .as_u64()
                    .is_some(),
                "{format}: {snapshot}"
            );
        }
        // The second logical request's snapshot describes exactly the list the
        // server received, including the tool the search loaded.
        let matching: Vec<&Value> = snapshots
            .iter()
            .filter(|snapshot| snapshot["number"] == 2)
            .collect();
        assert!(!matching.is_empty(), "{format}: no request 2 snapshot");
        for snapshot in &matching {
            assert_eq!(
                snapshot["tools"],
                Value::Array(sent_tools.clone()),
                "{format}: the snapshot lost a description or schema"
            );
        }
        // The recorded estimate must be measured from that same body, so
        // recomputing it from the captured request reproduces it exactly. A
        // catalog-wide measurement could not match.
        let protocol = match format {
            "anthropic" => "anthropic-messages",
            "responses" => "openai-responses",
            _ => "openai-completions",
        };
        let expected = serde_json::to_value(
            crate::native_engine::request_context::ContextUsage::from_body(
                protocol,
                &requests[1],
                None,
                512,
            ),
        )
        .unwrap();
        let estimated: Vec<&&Value> = matching
            .iter()
            .filter(|snapshot| !snapshot["contextUsage"].is_null())
            .collect();
        assert!(!estimated.is_empty(), "{format}: request 2 was not estimated");
        for snapshot in &estimated {
            assert_eq!(
                snapshot["contextUsage"], expected,
                "{format}: the estimate did not come from the sent body"
            );
        }
        // Loading a tool measurably grows the estimate, so descriptions and
        // schemas contribute rather than only names.
        let before = snapshots
            .iter()
            .find(|snapshot| snapshot["number"] == 1 && !snapshot["contextUsage"].is_null())
            .unwrap_or_else(|| panic!("{format}: request 1 was not estimated"));
        let after = estimated.last().unwrap();
        let bytes = |snapshot: &Value| {
            snapshot["contextUsage"]["tools"]["serialized_utf8_bytes"]
                .as_u64()
                .unwrap_or_default()
        };
        assert!(
            bytes(after) > bytes(before),
            "{format}: loading a tool did not grow the measured schema bytes"
        );
        drop(requests);
    }
}

/// Exact names preserve order and repeated loads leave the schema set alone.
#[tokio::test]
async fn tool_load_loads_multiple_exact_names_and_handles_already_loaded_names() {
    let root = tempfile::tempdir().unwrap();
    let mut executor = executor(root.path());
    let wanted = [
        "ListDir",
        "Monitor",
        "NotebookEdit",
        "TaskCreate",
        "TaskList",
    ];
    let (_, output, failed) = executor
        .execute_tool_call(
            "ToolLoad",
            &json!({"names":wanted.into_iter().chain(["ListDir", "Read"]).collect::<Vec<_>>()}),
            "load",
        )
        .await;
    assert!(!failed, "{output}");
    let result: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(result["loaded_tools"], json!(wanted));
    assert_eq!(result["already_available"], json!(["Read"]));
    assert_eq!(result["tools"].as_array().unwrap().len(), wanted.len());
    assert!(!result["tools"][0]["input_schema"].is_null());
    let (_, output, failed) = executor
        .execute_tool_call("ToolLoad", &json!({"names":["ListDir", "Read"]}), "repeat")
        .await;
    assert!(!failed, "{output}");
    let result: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(result["already_available"], json!(["ListDir", "Read"]));
    assert_eq!(result["loaded_tools"], json!([]));
    assert_eq!(result["tools"], json!([]));
}

#[tokio::test]
async fn tool_load_reports_unknown_and_unavailable_names_without_loading_a_partial_list() {
    let root = tempfile::tempdir().unwrap();
    write_switches(root.path(), &[("WebSearch", false, false)]);
    let mut executor = executor(root.path()).with_allowed_tools(Some(vec![
        "ToolLoad".into(),
        "ListDir".into(),
        "WebSearch".into(),
    ]));
    let input = json!({"names":[
        "ListDir", "WebSearch", "WebFetch", "listdir", "NoSuchTool",
        "select:ListDir", "web fetch", "ListDir,WebFetch", "git_status",
    ]});
    let (_, output, failed) = executor.execute_tool_call("ToolLoad", &input, "load").await;
    assert!(failed, "{output}");
    let result: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(
        result["unknown"],
        json!([
            "listdir",
            "NoSuchTool",
            "select:ListDir",
            "web fetch",
            "ListDir,WebFetch",
            "git_status",
        ])
    );
    assert_eq!(
        result["unavailable"],
        json!([
            {"name":"WebSearch","reason":"disabled by tool settings"},
            {"name":"WebFetch","reason":"not allowed for this agent"},
        ])
    );
    assert_eq!(result["loaded_tools"], json!([]));
    assert!(result["content"]
        .as_str()
        .unwrap()
        .contains("Unknown tools: listdir"));
    assert!(executor.loaded_tools.is_empty());
}

#[tokio::test]
async fn monitor_is_deferred_and_loadable() {
    let temp = tempfile::tempdir().unwrap();
    let mut executor =
        executor(temp.path()).with_conv_id(uuid::Uuid::new_v4().to_string());
    let initial = names(&executor);
    assert!(
        !initial.contains(&"Monitor".to_owned()),
        "Monitor must not be in the default request: {initial:?}"
    );
    executor.exposed_tools = Some(initial.into_iter().collect());
    let (_, output, failed) = executor
        .execute_tool_call("ToolLoad", &json!({"names":["Monitor"]}), "search")
        .await;
    assert!(!failed, "{output}");
    let loaded: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(loaded["success"], true, "{output}");
    assert_eq!(loaded["loaded_tools"], json!(["Monitor"]), "{output}");

    let next: std::collections::HashSet<String> = names(&executor).into_iter().collect();
    assert!(next.contains("Monitor"), "{next:?}");
}

#[tokio::test]
async fn file_memory_tool_dispatch_preserves_path_rules_hooks_roles_and_plan() {
    use crate::permissions::{rules::PermissionAction, PermissionMode, PermissionResult};
    use crate::services::{memory_files::FileMemory, EnvironmentCredentials, ServiceContext};
    let temp = tempfile::tempdir().unwrap();
    let paths = crate::runtime::RuntimePaths::new(temp.path().join("session-store")).unwrap();
    let service = ServiceContext::open(paths, Arc::new(EnvironmentCredentials)).unwrap();
    let session = crate::runtime::SessionStore::new(service.db.clone())
        .create(temp.path(), "fixture", "fixture")
        .unwrap();
    let memory = FileMemory::session(&service, &session).unwrap();
    let path = memory.directory.join("test.md");
    // The rule syntax treats a leading "/" as workspace-relative. On Unix an
    // absolute path therefore starts with "//" after this prefix; a Windows
    // drive path is already absolute. Strip the extended-length prefix the
    // canonical temp directory may carry: rules are written in user-facing
    // paths, and the matcher resolves the real path separately.
    let display = path.to_string_lossy().into_owned();
    let rule_path = if cfg!(windows) {
        display.strip_prefix(r"\\?\").unwrap_or(&display).to_owned()
    } else {
        format!("/{display}")
    };
    let input = json!({"file_path":path,"content":"---\nname: Testing\ndescription: Test preferences\nmetadata:\n  type: user\n---\nUse isolated fixtures\n"});
    let mut executor = executor(temp.path())
        .with_conv_id(session.clone())
        .with_file_memory(memory.clone());
    let pm = executor.permission_manager.clone().unwrap();
    pm.set_mode(PermissionMode::DontAsk);
    let (_, output, failed) = executor.execute_tool_call("Write", &input, "create").await;
    assert!(!failed, "{output}");
    assert!(path.exists());
    pm.set_session_rules(
        &session,
        HashMap::from([(
            format!("Write({rule_path})"),
            PermissionAction::Deny,
        )]),
    );
    let patch = json!({"patch":format!("*** Begin Patch\n*** Delete File: {}\n*** End Patch",path.display())});
    assert!(matches!(
        executor
            .check_permission_with("ApplyPatch", &patch, "patch")
            .await,
        PermissionResult::Denied(_)
    ));
    pm.set_mode(PermissionMode::AskPermissions);
    pm.set_session_rules(
        &session,
        HashMap::from([(
            format!("Write({rule_path})"),
            PermissionAction::Confirm,
        )]),
    );
    executor.approved_call = Some(tool_approval::ExactApproval {
        tool: "ApplyPatch".into(),
        input: patch.clone(),
        id: "patch".into(),
        pre_tool_hook: true,
    });
    assert!(matches!(
        executor
            .check_permission_with("ApplyPatch", &patch, "patch")
            .await,
        PermissionResult::RequiresConfirmation(_)
    ));
    executor.approved_call = None;
    pm.set_session_rules(&session, HashMap::new());
    executor.super_agent_role = crate::super_agent::role::Role::Router;
    let (_, output, failed) = executor.execute_tool_call("Write", &input, "router").await;
    assert!(failed && output.contains("Router"), "{output}");
    executor.super_agent_role = crate::super_agent::role::Role::Agent;
    pm.set_mode(PermissionMode::PlanMode);
    let (_, output, failed) = executor
        .execute_tool_call("Read", &json!({"file_path":path}), "plan-read")
        .await;
    assert!(!failed, "{output}");
    let (_, output, failed) = executor
        .execute_tool_call("Write", &input, "plan-write")
        .await;
    assert!(failed && output.contains("Plan mode"), "{output}");
    assert!(executor
        .memory_target(
            "Write",
            &json!({"file_path":memory.directory.join("../../other/memory/test.md")})
        )
        .is_err());
}
