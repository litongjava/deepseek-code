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
    .with_search_config_path(root.join("config.json"))
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
        "Grep",
        "Glob",
        "Skill",
        "ToolSearch",
        "AskUserQuestion",
        "EnterPlanMode",
        "ExitPlanMode",
        "TaskStop",
    ] {
        assert!(
            names.contains(&name.to_owned()),
            "{name} missing: {names:?}"
        );
    }
    for name in [
        "ListDir",
        "WebFetch",
        "WebSearch",
        "computer_use",
        "memory_search",
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
async fn tool_search_loads_the_real_definition_and_the_tool_runs_after_the_next_request() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("sample.txt"), "haystack").unwrap();
    let mut executor = executor(temp.path());
    let exposed: std::collections::HashSet<String> = names(&executor).into_iter().collect();
    executor.exposed_tools = Some(exposed);

    let (_, output, failed) = executor
        .execute_tool_call("ToolSearch", &json!({"query":"ListDir"}), "search")
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
        .execute_tool_call("ToolSearch", &json!({"query":"computer_use"}), "search")
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
async fn an_empty_search_reports_instead_of_dumping_the_catalog() {
    let temp = tempfile::tempdir().unwrap();
    let mut executor = executor(temp.path());
    let (_, output, failed) = executor
        .execute_tool_call("ToolSearch", &json!({"query":" "}), "search")
        .await;
    assert!(!failed, "{output}");
    let value: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(value["success"], false);
    assert_eq!(value["tools"], json!([]));
}

#[tokio::test]
async fn a_search_returns_at_most_five_tools_and_never_one_already_in_the_request() {
    let temp = tempfile::tempdir().unwrap();
    let mut executor = executor(temp.path());
    let exposed: std::collections::HashSet<String> = names(&executor).into_iter().collect();
    executor.exposed_tools = Some(exposed);
    let (_, output, _) = executor
        .execute_tool_call("ToolSearch", &json!({"query":"the"}), "search")
        .await;
    let value: Value = serde_json::from_str(&output).unwrap();
    let loaded = value["loaded_tools"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(!loaded.is_empty() && loaded.len() <= 5, "{output}");
    for name in loaded {
        let name = name.as_str().unwrap();
        assert!(
            !crate::services::customization::is_core_tool(name),
            "{name} was already in the request"
        );
    }
}

#[test]
fn only_a_paired_tool_search_result_restores_the_tools_of_this_session() {
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

    // A real ToolSearch call and its matching result.
    assert!(restore(vec![
        openai_call("c1", "ToolSearch"),
        openai_result("c1", &loaded(&["WebSearch"]))
    ])
    .contains(&"WebSearch".to_owned()));

    // The Anthropic shape read the same way.
    let anthropic = vec![
        json!({"role":"assistant","content":[{"type":"tool_use","id":"a1","name":"ToolSearch","input":{"query":"WebFetch"}}]}),
        json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"a1","content":loaded(&["WebFetch"]).to_string()}]}),
    ];
    assert!(restore(anthropic).contains(&"WebFetch".to_owned()));

    // An error result never loads, even with the right pairing.
    let errored = vec![
        json!({"role":"assistant","content":[{"type":"tool_use","id":"a2","name":"ToolSearch","input":{}}]}),
        json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"a2","is_error":true,"content":loaded(&["WebSearch"]).to_string()}]}),
    ];
    assert!(!restore(errored).contains(&"WebSearch".to_owned()));

    // An orphan result, a mismatched id, another tool's result and a user's own
    // JSON each fail to grant anything.
    assert!(!restore(vec![openai_result("c1", &loaded(&["WebSearch"]))])
        .contains(&"WebSearch".to_owned()));
    assert!(!restore(vec![
        openai_call("c1", "ToolSearch"),
        openai_result("c2", &loaded(&["WebSearch"]))
    ])
    .contains(&"WebSearch".to_owned()));
    assert!(!restore(vec![
        openai_call("c1", "Glob"),
        openai_result("c1", &loaded(&["WebSearch"]))
    ])
    .contains(&"WebSearch".to_owned()));
    assert!(!restore(vec![
        json!({"role":"user","content":loaded(&["WebSearch"]).to_string()})
    ])
    .contains(&"WebSearch".to_owned()));

    // A tool switched off after it was loaded does not come back.
    write_switches(temp.path(), &[("WebSearch", false, false)]);
    assert!(!restore(vec![
        openai_call("c1", "ToolSearch"),
        openai_result("c1", &loaded(&["WebSearch"]))
    ])
    .contains(&"WebSearch".to_owned()));
}

/// Stored history and old settings cannot resurrect a removed model tool: it is
/// no longer in the catalog, so it is neither restored nor sent, and a direct
/// call is rejected instead of being routed elsewhere.
#[tokio::test]
async fn removed_git_tools_cannot_be_restored_configured_or_called() {
    let temp = tempfile::tempdir().unwrap();
    let removed = ["git_status", "git_diff", "git_log", "git_add", "git_commit", "smart_project_scan"];
    // A stored successful ToolSearch result still names them, alongside a real
    // tool that must keep restoring.
    let stored = ["git_status", "git_add", "ListDir"];
    let mut restored = executor(temp.path());
    restored.messages = vec![
        json!({"role":"assistant","content":"","tool_calls":[{"id":"old","type":"function","function":{"name":"ToolSearch","arguments":"{\"query\":\"select:git_status,git_add\"}"}}]}),
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
    assert!(!names.contains(&"TaskGet".to_owned()), "{names:?}");
    assert!(!names.contains(&"TaskOutput".to_owned()), "{names:?}");
    assert!(crate::tools::get_tool_definitions()
        .iter()
        .all(|tool| tool.name != "TaskOutput"));
    let exposed: std::collections::HashSet<String> = names.into_iter().collect();
    probe.exposed_tools = Some(exposed);
    let (_, output, failed) = probe
        .execute_tool_call(
            "ToolSearch",
            &json!({"query":"select:TaskOutput"}),
            "search",
        )
        .await;
    assert!(!failed, "{output}");
    let value: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(value["loaded_tools"], json!([]), "{output}");
    assert_eq!(value["tools"], json!([]), "{output}");

    // TaskGet itself is deferred, but the same search still loads it.
    let (_, output, failed) = probe
        .execute_tool_call("ToolSearch", &json!({"query":"select:TaskGet"}), "search")
        .await;
    assert!(!failed, "{output}");
    let value: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(value["loaded_tools"], json!(["TaskGet"]), "{output}");
    assert_eq!(value["tools"].as_array().map(Vec::len), Some(1), "{output}");

    // A stored search result from before the consolidation cannot restore the
    // retired name either.
    let mut restored = executor(temp.path());
    restored.messages = vec![
        json!({"role":"assistant","content":[{"type":"tool_use","id":"a1","name":"ToolSearch","input":{}}]}),
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
        .execute_tool_call("ToolSearch", &json!({"query":"select:TaskGet"}), "search")
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
async fn task_get_is_deferred_and_loadable() {
    let temp = tempfile::tempdir().unwrap();
    let session = uuid::Uuid::new_v4().to_string();
    let mut executor = executor(temp.path()).with_conv_id(session.clone());
    let initial = names(&executor);
    assert!(!initial.contains(&"TaskGet".to_owned()), "{initial:?}");
    executor.exposed_tools = Some(initial.into_iter().collect());

    let (_, output, failed) = executor
        .execute_tool_call("ToolSearch", &json!({"query":"select:TaskGet"}), "search")
        .await;
    assert!(!failed, "{output}");
    let loaded: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(loaded["success"], true, "{output}");
    assert_eq!(loaded["loaded_tools"], json!(["TaskGet"]), "{output}");

    let task = crate::tools::tasks::execute_for_session(
        "TaskCreate",
        json!({"subject":"deferred task"}),
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
    assert_eq!(value["subject"], "deferred task");
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
        .execute_tool_call("ToolSearch", &json!({"query":"select:plan_mode"}), "search")
        .await;
    assert!(!failed, "{output}");
    let value: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(value["loaded_tools"], json!([]), "{output}");
    assert_eq!(value["tools"], json!([]), "{output}");

    // An old stored ToolSearch result cannot restore it.
    let mut restored = executor(temp.path());
    restored.messages = vec![
        json!({"role":"assistant","content":[{"type":"tool_use","id":"a1","name":"ToolSearch","input":{}}]}),
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
    let loaded = || json!({"success":true,"loaded_tools":["WebSearch"],"tools":[]}).to_string();
    let call = |role: &str, id: &str| json!({"role":role,"content":[{"type":"tool_use","id":id,"name":"ToolSearch","input":{}}]});
    let result = |id: &str| json!({"role":"user","content":[{"type":"tool_result","tool_use_id":id,"content":loaded()}]});
    let granted = |messages: Vec<Value>| restore(messages).contains(&"WebSearch".to_owned());

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
        names(&executor).contains(&"WebSearch".to_owned())
    };
    let loaded = || json!({"success":true,"loaded_tools":["WebSearch"],"tools":[]}).to_string();
    let openai_call = |id: &str, name: &str| json!({"role":"assistant","content":"","tool_calls":[{"id":id,"type":"function","function":{"name":name,"arguments":"{}"}}]});
    let openai_result =
        |id: &str, text: &str| json!({"role":"tool","tool_call_id":id,"content":text});
    let anth_call = |id: &str, name: &str| json!({"role":"assistant","content":[{"type":"tool_use","id":id,"name":name,"input":{}}]});
    let anth_result = |id: &str, text: &str, is_error: bool| json!({"role":"user","content":[{"type":"tool_result","tool_use_id":id,"content":text,"is_error":is_error}]});

    // A second call with the same id must not rename the first one's result.
    assert!(!granted(vec![
        openai_call("dup", "Read"),
        openai_call("dup", "ToolSearch"),
        openai_result("dup", &loaded()),
    ]));
    // The same on the Anthropic shape, within one assistant turn.
    assert!(!granted(vec![
        json!({"role":"assistant","content":[
            {"type":"tool_use","id":"dup2","name":"Read","input":{}},
            {"type":"tool_use","id":"dup2","name":"ToolSearch","input":{}}
        ]}),
        anth_result("dup2", &loaded(), false),
    ]));
    // A consumed id cannot be registered again to revive an old failure.
    assert!(!granted(vec![
        openai_call("once", "ToolSearch"),
        openai_result("once", "{\"success\":false,\"loaded_tools\":[]}"),
        openai_call("once", "ToolSearch"),
        openai_result("once", &loaded()),
    ]));
    assert!(!granted(vec![
        anth_call("once2", "ToolSearch"),
        anth_result("once2", "{\"success\":false,\"loaded_tools\":[]}", false),
        anth_call("once2", "ToolSearch"),
        anth_result("once2", &loaded(), false),
    ]));
    // A second result for one successful call still counts only once, and the
    // rule above must not break ordinary distinct ids in one turn.
    assert!(granted(vec![
        json!({"role":"assistant","content":[
            {"type":"tool_use","id":"ok1","name":"ToolSearch","input":{}},
            {"type":"tool_use","id":"ok2","name":"Read","input":{}}
        ]}),
        json!({"role":"user","content":[
            {"type":"tool_result","tool_use_id":"ok2","content":"file"},
            {"type":"tool_result","tool_use_id":"ok1","content":loaded()}
        ]}),
    ]));
    assert!(granted(vec![
        openai_call("turn1", "ToolSearch"),
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
    for name in ["Read", "Grep", "Glob", "ToolSearch"] {
        assert!(names.contains(&name.to_owned()), "{name}: {names:?}");
    }
    for name in ["ListDir", "WebFetch", "computer_use"] {
        assert!(!names.contains(&name.to_owned()), "{name}: {names:?}");
    }
}

#[tokio::test]
async fn a_closed_tool_search_cannot_be_called_directly() {
    let temp = tempfile::tempdir().unwrap();
    write_switches(temp.path(), &[("ToolSearch", false, true)]);
    let mut executor = executor(temp.path());
    assert!(!names(&executor).contains(&"ToolSearch".to_owned()));
    let (_, output, failed) = executor
        .execute_tool_call("ToolSearch", &json!({"query":"ListDir"}), "search")
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
async fn search_matches_and_returns_the_description_override() {
    let temp = tempfile::tempdir().unwrap();
    let mut customization = crate::services::customization::Customization::default();
    customization.tool_descriptions.insert(
        "ListDir".to_owned(),
        "Find archived recordings in a folder".to_owned(),
    );
    crate::services::customization::save(temp.path(), &customization, &[]).unwrap();
    let mut executor = executor(temp.path());
    let (_, output, failed) = executor
        .execute_tool_call(
            "ToolSearch",
            &json!({"query":"archived recordings"}),
            "search",
        )
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
fn a_legacy_entry_outside_the_catalog_still_turns_that_tool_off() {
    let temp = tempfile::tempdir().unwrap();
    let customization = crate::services::customization::Customization {
        disabled_tools: vec!["mcp__old__echo".into()],
        ..Default::default()
    };
    crate::services::customization::save(temp.path(), &customization, &[]).unwrap();
    let mut executor = executor(temp.path());
    executor.mcp_tools = vec![crate::tools::ToolDefinition {
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
        name: "mcp__live__echo".into(),
        description: "live".into(),
        input_schema: json!({"type":"object"}),
    });
    assert!(names(&executor).contains(&"mcp__live__echo".to_owned()));
    assert!(executor.loading_violation("mcp__live__echo").is_none());
}

#[test]
fn a_restricted_agent_without_discovery_still_receives_its_granted_tools() {
    let temp = tempfile::tempdir().unwrap();
    let mut executor = executor(temp.path());
    executor.allowed_tools = Some(vec!["Read".into(), "ListDir".into()]);
    let visible = names(&executor);
    assert!(visible.contains(&"ListDir".to_owned()), "{visible:?}");
    assert!(visible.contains(&"Read".to_owned()), "{visible:?}");

    // With ToolSearch granted the agent keeps the normal on-demand rule.
    executor.allowed_tools = Some(vec!["Read".into(), "ListDir".into(), "ToolSearch".into()]);
    assert!(!names(&executor).contains(&"ListDir".to_owned()));
}

#[test]
fn damaged_settings_fail_the_request_instead_of_reopening_every_tool() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(temp.path().join("customization.json"), "{broken").unwrap();
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
        json!({"role":"assistant","content":"","tool_calls":[{"id":"s1","type":"function","function":{"name":"ToolSearch","arguments":"{\"query\":\"ListDir\"}"}}]}),
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
    .with_search_config_path(root.path().join("config.json"));
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
    .with_search_config_path(root.path().join("config.json"));
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
async fn every_transport_adds_a_searched_tool_to_the_next_request() {
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
                            "ToolSearch",
                            json!({"query":"ListDir WebFetch"}),
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
        .with_search_config_path(root.path().join("config.json"));
        let (text, _) = tokio::time::timeout(Duration::from_secs(20), executor.execute())
            .await
            .unwrap()
            .unwrap();
        let failed = |rx: &mut mpsc::Receiver<EngineEvent>| {
            let mut errors = Vec::new();
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
                }
            }
            errors
        };
        assert!(failed(&mut rx).is_empty(), "{format}");
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
            second.contains(&"WebFetch".to_owned()),
            "{format}: {second:?}"
        );
        assert!(
            requests[1]["tools"].to_string().contains("\"path\""),
            "{format}"
        );
        assert!(requests[2].to_string().contains("needle.txt"), "{format}");
    }
}

/// The real request payloads on all three transports carry a directory built
/// from the current switches, and it changes on the next request after a load
/// or a settings change. Resident, disabled, removed, MCP and custom tools are
/// not listed.
#[tokio::test]
async fn every_transport_sends_a_dynamic_directory_of_searchable_builtins() {
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
                                "ToolSearch",
                                json!({"query":"ListDir"}),
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
        .with_search_config_path(root.path().join("config.json"));
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
        assert_eq!(
            first.matches("# Tool directory").count(),
            1,
            "{format}: {first}"
        );
        assert!(
            first.contains("can load only the built-in tools listed here"),
            "{format}: {first}"
        );
        assert!(first.contains("`ListDir`"), "{format}: {first}");
        assert!(
            first.contains("`ListDir`: Directory listing from the shared snapshot"),
            "{format}: the directory uses the configured override: {first}"
        );
        assert!(first.contains("`WebSearch`"), "{format}: {first}");
        assert!(first.contains("`MultiEdit`"), "{format}: {first}");
        for absent in [
            "`Bash`",
            "`WebFetch`",
            "`computer_use`",
            "`private_tool`",
            "`git_status`",
            "`git_diff`",
        ] {
            assert!(!first.contains(absent), "{format}: {absent} in {first}");
        }
        assert!(!request_tool_names(&requests[0], format).contains(&"ListDir".to_owned()));

        let second = request_system_prompt(&requests[1], format);
        assert!(second.contains("BASE SYSTEM"), "{format}: {second}");
        assert!(
            !second.contains("`ListDir`"),
            "{format}: a loaded tool must leave the directory: {second}"
        );
        assert!(
            !second.contains("`WebSearch`"),
            "{format}: a disabled tool must leave the directory: {second}"
        );
        assert!(second.contains("`MultiEdit`"), "{format}: {second}");
        assert!(request_tool_names(&requests[1], format).contains(&"ListDir".to_owned()));
        // The loaded schema carries the same override the directory used:
        // both were built from one settings snapshot.
        assert!(
            requests[1]["tools"]
                .to_string()
                .contains("Directory listing from the shared snapshot"),
            "{format}: {second}"
        );
    }
}

/// An empty searchable set is reported instead of an empty or misleading list,
/// and a request without ToolSearch says so.
#[tokio::test]
async fn an_empty_directory_is_reported_truthfully() {
    use axum::{http::header, routing::post, Json, Router};
    let _ = crate::config::OrchestrationConfig::init_global();
    for (allowed, expected) in [
        (
            vec!["Bash", "ToolSearch"],
            "No built-in tools are available for ToolSearch to load",
        ),
        (
            vec!["Bash"],
            "ToolSearch is not available in this request",
        ),
    ] {
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
        let mut executor = ToolLoopExecutor::new(
            wire_provider(&base, "openai"),
            vec![json!({"role":"user","content":"hello"})],
            Some("BASE SYSTEM".into()),
            512,
            tx,
            root.path().to_string_lossy().into_owned(),
            ReasoningMode::Standard,
        )
        .with_search_config_path(root.path().join("config.json"))
        .with_allowed_tools(Some(
            allowed.iter().map(|name| (*name).to_owned()).collect(),
        ));
        let (text, _) = tokio::time::timeout(Duration::from_secs(20), executor.execute())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(text, "done");
        server.abort();
        let requests = captured.lock().await;
        let system = request_system_prompt(&requests[0], "openai");
        assert!(system.contains(expected), "{system}");
        assert!(!system.contains("- `"), "{system}");
    }
}

/// The next request's tool result as the model reads it, for the OpenAI wire
/// format used by the selection tests.
fn openai_tool_result(body: &Value) -> Value {
    body["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .find(|message| message["role"] == "tool")
        .and_then(|message| message["content"].as_str())
        .and_then(|text| serde_json::from_str(text).ok())
        .expect("the tool result in the next request")
}

/// Drives one real OpenAI-compatible wire turn whose model calls
/// `ToolSearch(query)`, returning the captured request bodies. The caller
/// supplies the tool switches for the fixture data directory.
async fn tool_search_requests_over_the_wire(
    root: &std::path::Path,
    switches: &[(&str, bool, bool)],
    query: &str,
) -> Vec<Value> {
    use axum::{http::header, routing::post, Json, Router};
    let _ = crate::config::OrchestrationConfig::init_global();
    write_switches(root, switches);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
    let requests = captured.clone();
    let query = query.to_owned();
    let app = Router::new().route(
        "/v1/chat/completions",
        post(move |Json(body): Json<Value>| {
            let requests = requests.clone();
            let query = query.clone();
            async move {
                let step = {
                    let mut seen = requests.lock().await;
                    seen.push(body);
                    seen.len() - 1
                };
                let events = match step {
                    0 => tool_call_events("openai", "search", "ToolSearch", json!({"query":query})),
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
    let (tx, _rx) = mpsc::channel(64);
    let mut executor = ToolLoopExecutor::new(
        wire_provider(&base, "openai"),
        vec![json!({"role":"user","content":"Load the tools"})],
        None,
        512,
        tx,
        root.to_string_lossy().into_owned(),
        ReasoningMode::Standard,
    )
    .with_search_config_path(root.join("config.json"));
    let (text, _) = tokio::time::timeout(Duration::from_secs(20), executor.execute())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(text, "done");
    server.abort();
    let bodies = captured.lock().await.clone();
    bodies
}

/// An explicit selection loads every eligible name it lists, however the names
/// are separated, and reports unknown or disabled names instead of dropping
/// them. Keyword discovery keeps its own five-result bound.
#[tokio::test]
async fn explicit_selection_loads_every_eligible_name_and_reports_the_rest() {
    use axum::{http::header, routing::post, Json, Router};
    let _ = crate::config::OrchestrationConfig::init_global();
    let root = tempfile::tempdir().unwrap();
    let selected = [
        "ListDir",
        "MultiEdit",
        "WebFetch",
        "WebSearch",
        "TaskCreate",
        "TaskGet",
        "TaskList",
    ];
    write_switches(
        root.path(),
        &[
            ("ListDir", false, true),
            ("MultiEdit", false, true),
            ("WebFetch", false, true),
            ("WebSearch", false, true),
            ("TaskCreate", false, true),
            ("TaskGet", false, true),
            ("TaskList", false, true),
            ("TaskUpdate", false, false),
        ],
    );
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
                // Commas, whitespace, a mixed form and a duplicate spelling.
                let query = "select:ListDir, MultiEdit,WebFetch WebSearch,TaskCreate,TaskGet,TaskList,TaskUpdate,NoSuchTool,listdir";
                let events = match step {
                    0 => tool_call_events("openai", "select-many", "ToolSearch", json!({"query":query})),
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
    let (tx, _rx) = mpsc::channel(64);
    let mut executor = ToolLoopExecutor::new(
        wire_provider(&base, "openai"),
        vec![json!({"role":"user","content":"Load the tools"})],
        None,
        512,
        tx,
        root.path().to_string_lossy().into_owned(),
        ReasoningMode::Standard,
    )
    .with_search_config_path(root.path().join("config.json"));
    let (text, _) = tokio::time::timeout(Duration::from_secs(20), executor.execute())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(text, "done");
    server.abort();

    let requests = captured.lock().await;
    assert_eq!(requests.len(), 2, "one search request and one follow-up");
    let first = request_system_prompt(&requests[0], "openai");
    for name in selected {
        assert!(first.contains(&format!("`{name}`")), "{name}: {first}");
    }
    assert!(!first.contains("`TaskUpdate`"), "{first}");
    assert!(!first.contains("`NoSuchTool`"), "{first}");

    let second = request_tool_names(&requests[1], "openai");
    for name in selected {
        assert!(second.contains(&name.to_owned()), "{name}: {second:?}");
    }
    assert!(!second.contains(&"TaskUpdate".to_owned()), "{second:?}");
    assert!(!second.contains(&"NoSuchTool".to_owned()), "{second:?}");

    let result = openai_tool_result(&requests[1]);
    assert_eq!(
        result["loaded_tools"],
        json!(["ListDir", "MultiEdit", "WebFetch", "WebSearch", "TaskCreate", "TaskGet", "TaskList"]),
        "{result}"
    );
    assert_eq!(result["unknown"], json!(["NoSuchTool"]), "{result}");
    assert_eq!(
        result["unavailable"],
        json!([{"name":"TaskUpdate","reason":"disabled by tool settings"}]),
        "{result}"
    );

    // Every loaded tool left the next directory; the remaining searchable
    // built-ins are still advertised.
    let directory = request_system_prompt(&requests[1], "openai");
    assert!(directory.contains("# Tool directory"), "{directory}");
    assert!(directory.contains("- `"), "{directory}");
    for name in selected {
        assert!(
            !directory.contains(&format!("`{name}`")),
            "{name} stayed in the directory: {directory}"
        );
    }
    assert!(!directory.contains("`TaskUpdate`"), "{directory}");
}

/// A whitespace-separated list that also names an unknown, removed or disabled
/// tool still loads every eligible name. Keyword matching finds nothing, so an
/// exact-case name token switches to an explicit selection; the next real HTTP
/// request carries the loaded schemas and the result reports the rest.
#[tokio::test]
async fn space_separated_names_load_when_mixed_with_unknown_or_removed_names() {
    const ELIGIBLE: [&str; 7] = [
        "ListDir",
        "MultiEdit",
        "WebFetch",
        "WebSearch",
        "TaskCreate",
        "TaskGet",
        "TaskList",
    ];
    let switches = [
        ("ListDir", false, true),
        ("MultiEdit", false, true),
        ("WebFetch", false, true),
        ("WebSearch", false, true),
        ("TaskCreate", false, true),
        ("TaskGet", false, true),
        ("TaskList", false, true),
        ("TaskUpdate", false, false),
    ];
    let cases: [(&str, String, Vec<&str>, Value, Value); 3] = [
        (
            "space plus unknown",
            format!("{} NoSuchTool", ELIGIBLE.join(" ")),
            ELIGIBLE.to_vec(),
            json!(["NoSuchTool"]),
            json!([]),
        ),
        (
            "space plus removed",
            "ListDir git_status".into(),
            vec!["ListDir"],
            json!(["git_status"]),
            json!([]),
        ),
        (
            "space plus disabled",
            "ListDir TaskUpdate".into(),
            vec!["ListDir"],
            json!([]),
            json!([{"name":"TaskUpdate","reason":"disabled by tool settings"}]),
        ),
    ];
    for (label, query, expected_loaded, expected_unknown, expected_unavailable) in cases {
        let root = tempfile::tempdir().unwrap();
        let requests = tool_search_requests_over_the_wire(root.path(), &switches, &query).await;
        assert_eq!(requests.len(), 2, "{label}: one search and one follow-up");
        let second = request_tool_names(&requests[1], "openai");
        for name in &expected_loaded {
            assert!(
                second.contains(&name.to_string()),
                "{label}: {name} missing from the next request: {second:?}"
            );
        }
        for absent in ["NoSuchTool", "git_status", "TaskUpdate"] {
            assert!(
                !second.contains(&absent.to_owned()),
                "{label}: {absent} stayed out of the next request: {second:?}"
            );
        }
        let result = openai_tool_result(&requests[1]);
        assert_eq!(
            result["loaded_tools"],
            json!(expected_loaded),
            "{label}: {result}"
        );
        assert_eq!(result["unknown"], expected_unknown, "{label}: {result}");
        assert_eq!(
            result["unavailable"], expected_unavailable,
            "{label}: {result}"
        );
        assert_eq!(
            result["success"],
            !expected_loaded.is_empty(),
            "{label}: {result}"
        );
    }
}

/// Discovery is advertised only when ToolSearch is actually in the request:
/// both switches off, and on-demand-only but not yet loaded, both send a
/// truthful directory and no callable ToolSearch.
#[tokio::test]
async fn a_request_without_callable_tool_search_does_not_promise_loading() {
    use axum::{http::header, routing::post, Json, Router};
    let _ = crate::config::OrchestrationConfig::init_global();
    for (label, enabled, on_demand) in [("both off", false, false), ("on demand only", false, true)] {
        let root = tempfile::tempdir().unwrap();
        write_switches(root.path(), &[("ToolSearch", enabled, on_demand)]);
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
        .with_search_config_path(root.path().join("config.json"));
        let (text, _) = tokio::time::timeout(Duration::from_secs(20), executor.execute())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(text, "done", "{label}");
        server.abort();
        let requests = captured.lock().await;
        let sent = request_tool_names(&requests[0], "openai");
        assert!(!sent.contains(&"ToolSearch".to_owned()), "{label}: {sent:?}");
        assert!(sent.contains(&"Bash".to_owned()), "{label}: {sent:?}");
        let system = request_system_prompt(&requests[0], "openai");
        assert!(
            system.contains("ToolSearch is not available in this request"),
            "{label}: {system}"
        );
        assert!(!system.contains("- `"), "{label}: {system}");
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
                        "ToolSearch",
                        json!({"query":"Write"}),
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
    .with_search_config_path(root.path().join("config.json"))
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
                        "ToolSearch",
                        json!({"query":"Write"}),
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
    .with_search_config_path(root.path().join("config.json"))
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
                        "ToolSearch",
                        json!({"query":"Bash"}),
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
    .with_search_config_path(root.path().join("config.json"))
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
                                    "ToolSearch",
                                    json!({"query":"ListDir"}),
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
        .with_search_config_path(root.path().join("config.json"));
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
                            "search-1",
                            "ToolSearch",
                            json!({"query":"ListDir"}),
                        ),
                        1 => {
                            match case {
                                "double_off" => write_switches(&dir, &[("ListDir", false, false)]),
                                "enabled_without_on_demand" => {
                                    write_switches(&dir, &[("ListDir", true, false)])
                                }
                                _ => std::fs::write(dir.join("customization.json"), b"{").unwrap(),
                            }
                            tool_call_events(
                                "openai",
                                "list-1",
                                "ListDir",
                                json!({"path":"."}),
                            )
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
        .with_search_config_path(root.path().join("config.json"))
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
                    !request_tool_names(&requests[2], "openai")
                        .contains(&"ListDir".to_owned()),
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
                assert!(request_tool_names(&requests[2], "openai")
                    .contains(&"ListDir".to_owned()));
            }
            _ => {
                assert!(result.is_err(), "a damaged file must fail the request");
                assert_eq!(rejected.len(), 1, "{rejected:?}");
                assert!(
                    rejected[0].contains("could not be read"),
                    "{rejected:?}"
                );
                assert_eq!(
                    requests.len(),
                    2,
                    "the next request must not be sent with damaged settings"
                );
            }
        }
    }
}

#[tokio::test]
async fn search_matches_separate_keywords_and_exact_selections() {
    let temp = tempfile::tempdir().unwrap();
    let mut executor = executor(temp.path());
    executor.exposed_tools = Some(names(&executor).into_iter().collect());
    let loaded = |output: &str| -> Vec<String> {
        let value: Value = serde_json::from_str(output).unwrap();
        value["loaded_tools"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|name| name.as_str().map(str::to_owned))
            .collect()
    };
    // Words in a different form than the name still find it first.
    let (_, output, _) = executor
        .execute_tool_call("ToolSearch", &json!({"query":"web fetch"}), "words")
        .await;
    assert_eq!(
        loaded(&output).first().map(String::as_str),
        Some("WebFetch"),
        "{output}"
    );
    // An exact selection loads only the named tools, in the requested order.
    let (_, output, _) = executor
        .execute_tool_call(
            "ToolSearch",
            &json!({"query":"select:WebSearch, ListDir"}),
            "select",
        )
        .await;
    assert_eq!(loaded(&output), vec!["WebSearch", "ListDir"], "{output}");
    // A word that matches nothing keeps the whole query from matching.
    let (_, output, _) = executor
        .execute_tool_call("ToolSearch", &json!({"query":"web zzqx"}), "miss")
        .await;
    assert!(loaded(&output).is_empty(), "{output}");
    // Removed model tools are not part of the catalog and cannot be loaded by
    // name, keyword or an exact selection. Another tool may still match a
    // generic keyword, but never one of the removed names.
    for query in [
        "git_status",
        "git diff",
        "select:git_add,git_commit",
        "smart_project_scan",
    ] {
        let (_, output, _) = executor
            .execute_tool_call("ToolSearch", &json!({"query":query}), "removed")
            .await;
        let names = loaded(&output);
        for removed in [
            "git_status",
            "git_diff",
            "git_log",
            "git_add",
            "git_commit",
            "smart_project_scan",
        ] {
            assert!(
                !names.contains(&removed.to_owned()),
                "{query} loaded {removed}: {output}"
            );
        }
    }
}

/// Keyword discovery keeps its previous behaviour, including when a query
/// contains tool-name words in ordinary prose or in the wrong case. The list
/// fallback only applies when keyword matching finds nothing and a token is an
/// exact-case catalog name, so these queries must stay keyword searches.
#[tokio::test]
async fn keyword_searches_are_not_turned_into_name_lists() {
    for (query, first) in [
        ("web fetch", "WebFetch"),
        ("read notebook", "NotebookEdit"),
        ("read file", "NotebookEdit"),
        ("task list", "TaskList"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let mut executor = executor(temp.path());
        executor.exposed_tools = Some(names(&executor).into_iter().collect());
        let (_, output, _) = executor
            .execute_tool_call("ToolSearch", &json!({"query":query}), "keyword")
            .await;
        let value: Value = serde_json::from_str(&output).unwrap();
        let loaded: Vec<&str> = value["loaded_tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|name| name.as_str())
            .collect();
        assert_eq!(loaded.first().copied(), Some(first), "{query}: {output}");
        // Keyword results never report per-name unknown or unavailable entries.
        assert!(value.get("unknown").is_none(), "{query}: {output}");
        assert!(value.get("unavailable").is_none(), "{query}: {output}");
        // Keyword discovery keeps its five-result bound.
        assert!(loaded.len() <= 5, "{query}: {output}");
    }
    // No keyword hit and no exact-case catalog name: the query stays an
    // ordinary failed search. Lowercase complete names deliberately do not
    // trigger the list fallback.
    for query in ["read zzqx", "listdir git_status"] {
        let temp = tempfile::tempdir().unwrap();
        let mut executor = executor(temp.path());
        executor.exposed_tools = Some(names(&executor).into_iter().collect());
        let (_, output, _) = executor
            .execute_tool_call("ToolSearch", &json!({"query":query}), "miss")
            .await;
        let value: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(value["loaded_tools"], json!([]), "{query}: {output}");
        assert_eq!(value["success"], false, "{query}: {output}");
        assert!(value.get("unknown").is_none(), "{query}: {output}");
        assert!(
            value["content"]
                .as_str()
                .unwrap()
                .contains("No tool available on demand matches"),
            "{query}: {output}"
        );
    }
}

#[tokio::test]
async fn search_selects_complete_names_with_commas_and_whitespace() {
    for (query, expected) in [
        ("WebSearch WebFetch", vec!["WebSearch", "WebFetch"]),
        ("WebSearch,WebFetch", vec!["WebSearch", "WebFetch"]),
        ("  WebSearch, \tWebFetch  ", vec!["WebSearch", "WebFetch"]),
        ("websearch WEBFETCH WebSearch", vec!["WebSearch", "WebFetch"]),
        ("ListDir,WebSearch WebFetch,ListDir", vec!["ListDir", "WebSearch", "WebFetch"]),
        ("select:WebSearch WebFetch", vec!["WebSearch", "WebFetch"]),
        ("select:WebSearch, WebFetch,WebSearch", vec!["WebSearch", "WebFetch"]),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let mut executor = executor(temp.path());
        executor.exposed_tools = Some(names(&executor).into_iter().collect());
        let (_, output, error) = executor
            .execute_tool_call("ToolSearch", &json!({"query":query}), "names")
            .await;
        let value: Value = serde_json::from_str(&output).unwrap();
        assert!(!error, "{query}: {output}");
        assert_eq!(
            value["loaded_tools"],
            json!(expected),
            "{query}"
        );
        for name in expected {
            assert!(
                names(&executor).contains(&name.to_owned()),
                "{query}: {name}"
            );
        }
    }
}

#[tokio::test]
async fn complete_name_selection_preserves_loading_restrictions() {
    for restriction in ["disabled", "exposed", "not_allowed"] {
        let temp = tempfile::tempdir().unwrap();
        let mut executor = executor(temp.path());
        if restriction == "disabled" {
            write_switches(temp.path(), &[("WebSearch", false, false)]);
        }
        let mut exposed: std::collections::HashSet<String> = names(&executor).into_iter().collect();
        if restriction == "exposed" {
            exposed.insert("WebSearch".into());
        }
        executor.exposed_tools = Some(exposed);
        if restriction == "not_allowed" {
            executor.allowed_tools = Some(vec!["ToolSearch".into(), "WebFetch".into()]);
        }
        let (_, output, error) = executor
            .execute_tool_call(
                "ToolSearch",
                &json!({"query":"WebSearch WebFetch"}),
                "limited",
            )
            .await;
        let value: Value = serde_json::from_str(&output).unwrap();
        assert!(!error, "{restriction}: {output}");
        assert_eq!(value["loaded_tools"], json!(["WebFetch"]), "{restriction}");
    }
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
        .execute_tool_call("ToolSearch", &json!({"query":"select:Monitor"}), "search")
        .await;
    assert!(!failed, "{output}");
    let loaded: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(loaded["success"], true, "{output}");
    assert_eq!(loaded["loaded_tools"], json!(["Monitor"]), "{output}");

    let next: std::collections::HashSet<String> = names(&executor).into_iter().collect();
    assert!(next.contains("Monitor"), "{next:?}");
}
