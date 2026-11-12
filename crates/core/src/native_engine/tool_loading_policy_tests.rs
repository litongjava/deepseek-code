//! The switches as the engine enforces them: what a request carries, what a
//! call in that request may run, and what a ToolSearch can load.
use super::*;
use crate::services::customization::{ToolLoading, CORE_TOOLS};

fn provider() -> ResolvedProvider {
    ResolvedProvider {
        provider: serde_json::from_value(json!({"id":"fixture","name":"fixture","base_url":"http://127.0.0.1:1","api_key":"unused","api_format":"openai","models":[],"enabled":true})).unwrap(),
        model: serde_json::from_value(json!({"id":"fixture","name":"fixture","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap(),
    }
}

/// An executor whose data directory is `root`, so `customization.json` there is
/// the file the engine reads, as it would be from the Desktop or the CLI.
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

/// Drive the public executor through real HTTP, never rebuilding its tools in
/// the test. Settings change while the mock is answering an in-flight request.
async fn provider_loop_case(format: &'static str, scenario: &'static str) {
    use axum::{http::header, response::IntoResponse, routing::post, Json, Router};
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("execution-marker.txt"), "fixture").unwrap();
    if scenario == "hot" {
        write_switches(root.path(), &[("ListDir", true, false)]);
    } else if scenario == "permission" {
        write_switches(root.path(), &[("Write", false, true)]);
    }
    let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
    let received = captured.clone();
    let settings_root = root.path().to_owned();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let route = match format {
        "anthropic" => "/v1/messages",
        "responses" => "/v1/responses",
        _ => "/v1/chat/completions",
    };
    let app = Router::new().route(
        route,
        post(move |Json(body): Json<Value>| {
            let received = received.clone();
            let root = settings_root.clone();
            async move {
                let mut requests = received.lock().await;
                let step = requests.len();
                requests.push(body);
                if scenario == "retry" && step == 0 {
                    write_switches(&root, &[("Read", false, false)]);
                    return (
                        axum::http::StatusCode::SERVICE_UNAVAILABLE,
                        [("retry-after", "0")],
                        "retry fixture",
                    )
                        .into_response();
                }
                let call = match (scenario, step) {
                    ("restore" | "restore_off" | "restore_restricted" | "restore_untrusted", 0) => {
                        Some(("ListDir", json!({"path":"."})))
                    }
                    ("search", 0) => Some(("ToolSearch", json!({"query":"ListDir"}))),
                    ("search", 1) => Some(("ListDir", json!({"path":"."}))),
                    ("permission", 0) => Some(("ToolSearch", json!({"query":"Write"}))),
                    ("permission", 1) => Some((
                        "Write",
                        json!({"file_path":"forbidden.txt","content":"must not execute"}),
                    )),
                    ("hot", 0) => {
                        write_switches(&root, &[("ListDir", false, false)]);
                        Some(("ListDir", json!({"path":"."})))
                    }
                    ("hot", 1) => {
                        write_switches(&root, &[("ListDir", true, false), ("Read", false, false)]);
                        Some(("Read", json!({"file_path":"execution-marker.txt"})))
                    }
                    ("hot", 2) => Some(("ListDir", json!({"path":"."}))),
                    ("corrupt", 0) => {
                        std::fs::write(root.join("customization.json"), "{broken").unwrap();
                        Some(("Read", json!({"file_path":"execution-marker.txt"})))
                    }
                    _ => None,
                };
                (
                    [(header::CONTENT_TYPE, "text/event-stream")],
                    provider_reply(format, step, call),
                )
                    .into_response()
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let (tx, mut rx) = mpsc::channel(512);
    let mut executor = executor(root.path());
    executor.event_tx = tx;
    if scenario == "permission" {
        executor
            .permission_manager
            .as_ref()
            .unwrap()
            .set_mode(crate::permissions::PermissionMode::PlanMode);
    }
    executor.provider.provider.base_url = if format == "responses" {
        format!("{endpoint}/v1")
    } else {
        endpoint
    };
    executor.provider.provider.api_format = if format == "anthropic" {
        ApiFormat::Anthropic
    } else {
        ApiFormat::OpenAI
    };
    if format == "responses" {
        executor.provider.provider.builtin_id = Some("openai".into());
        executor.provider.model = serde_json::from_value(json!({"id":"fixture","name":"fixture","enabled":true,"api":"openai-responses","supports_web_search":false})).unwrap();
    }
    executor.messages = vec![json!({"role":"user","content":"Use the fixture tools"})];
    if scenario.starts_with("restore") {
        executor.conv_id = Some("10000000-0000-4000-8000-000000000001".into());
        executor = executor.with_goal_state(persisted_search(
            root.path(),
            scenario == "restore_untrusted",
        ));
        if scenario == "restore_off" {
            write_switches(root.path(), &[("ListDir", false, false)]);
        } else if scenario == "restore_restricted" {
            executor.allowed_tools = Some(vec!["Read".into(), "ToolSearch".into()]);
        }
        // Desktop supplies only display text after reopening a conversation.
        executor.messages = vec![
            json!({"role":"user","content":"search"}),
            json!({"role":"assistant","content":"done"}),
            json!({"role":"user","content":"continue"}),
        ];
    }
    let result = tokio::time::timeout(Duration::from_secs(20), executor.execute()).await;
    server.abort();
    let result = result.expect("provider loop timed out");
    let requests = captured.lock().await;
    let expected = match scenario {
        "search" | "permission" => 3,
        "hot" => 4,
        "retry" => 2,
        "restore" | "restore_off" | "restore_restricted" | "restore_untrusted" => 2,
        _ => 1,
    };
    assert_eq!(requests.len(), expected, "{format} {scenario}: {result:?}");
    if scenario == "corrupt" {
        assert!(
            result.is_err(),
            "damaged settings must stop before the next HTTP request"
        );
    } else {
        assert_eq!(result.unwrap().0, "verified");
    }
    let tool = |step: usize, name: &str| -> Option<Value> {
        requests[step]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find_map(|tool| {
                let definition = if format == "openai" {
                    &tool["function"]
                } else {
                    tool
                };
                (definition["name"] == name).then(|| definition.clone())
            })
    };
    if scenario == "search" {
        assert!(tool(0, "ListDir").is_none());
        let definition = tool(1, "ListDir").expect("search must update the next HTTP schema");
        let schema = if format == "anthropic" {
            &definition["input_schema"]
        } else {
            &definition["parameters"]
        };
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["path"].is_object(), "{definition}");
    } else if scenario == "hot" {
        assert!(tool(0, "ListDir").is_some());
        assert!(tool(1, "ListDir").is_none());
        assert!(tool(2, "ListDir").is_some());
        assert!(tool(2, "Read").is_none());
    }
    let mut calls = Vec::new();
    let mut snapshots = Vec::new();
    while let Ok(event) = rx.try_recv() {
        match event {
            EngineEvent::ToolUseDone {
                tool_name,
                output,
                is_error,
                ..
            } => calls.push((tool_name, output, is_error)),
            EngineEvent::RequestSnapshot(snapshot)
                if snapshot["status"] == "running"
                    && !snapshots
                        .iter()
                        .any(|previous: &Value| previous["id"] == snapshot["id"]) =>
            {
                snapshots.push(snapshot)
            }
            _ => {}
        }
    }
    assert_eq!(
        snapshots.len(),
        if scenario == "retry" {
            1
        } else {
            requests.len()
        },
        "one snapshot per logical request"
    );
    for (snapshot, request) in snapshots.iter().zip(requests.iter()) {
        let wire_names: Vec<_> = request["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| {
                if format == "openai" {
                    tool["function"]["name"].clone()
                } else {
                    tool["name"].clone()
                }
            })
            .collect();
        let snapshot_names: Vec<_> = snapshot["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].clone())
            .collect();
        assert_eq!(
            snapshot_names, wire_names,
            "snapshot and HTTP tools must agree"
        );
    }
    if scenario.starts_with("restore") {
        assert_eq!(
            tool(0, "ListDir").is_some(),
            scenario == "restore",
            "{format} {scenario}: durable loading must reach the real first HTTP request"
        );
        let (_, output, failed) = calls
            .iter()
            .find(|(name, _, _)| name == "ListDir")
            .expect("the forced call is recorded");
        assert_eq!(*failed, scenario != "restore", "{output}");
        assert_eq!(
            output.contains("execution-marker.txt"),
            scenario == "restore",
            "{output}"
        );
    } else if scenario == "permission" {
        assert!(tool(0, "Write").is_none());
        assert!(tool(1, "Write").is_some());
        assert!(
            calls
                .iter()
                .any(|(name, _, failed)| name == "ToolSearch" && !failed),
            "{calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|(name, _, failed)| name == "Write" && *failed),
            "{calls:?}"
        );
        assert!(!root.path().join("forbidden.txt").exists());
    } else if scenario == "retry" {
        assert_eq!(
            requests[0]["tools"], requests[1]["tools"],
            "HTTP retries must freeze schemas despite config changes"
        );
        assert!(calls.is_empty());
    } else if scenario == "corrupt" {
        assert!(
            calls
                .iter()
                .any(|(_, output, failed)| *failed && output.contains("could not be read")),
            "{calls:?}"
        );
    } else {
        assert!(
            calls.iter().any(|(name, output, failed)| name == "ListDir"
                && !failed
                && output.contains("execution-marker.txt")),
            "actual execution: {calls:?}"
        );
        if scenario == "search" {
            assert!(
                calls
                    .iter()
                    .any(|(name, output, failed)| name == "ToolSearch"
                        && !failed
                        && output.contains("loaded_tools")),
                "{calls:?}"
            );
        } else {
            assert_eq!(
                calls.iter().filter(|(_, _, failed)| *failed).count(),
                2,
                "{calls:?}"
            );
        }
    }
}

fn persisted_search(root: &std::path::Path, untrusted_only: bool) -> Arc<crate::db::DbManager> {
    let path = root.join("durable.db");
    {
        let db = Arc::new(crate::db::DbManager::new(path.clone()).unwrap());
        db.init().unwrap();
        let sessions = crate::runtime::SessionStore::new(db.clone());
        for session in [
            "10000000-0000-4000-8000-000000000001",
            "10000000-0000-4000-8000-000000000002",
        ] {
            sessions
                .create_with_id(root, "fixture", "fixture", session)
                .unwrap();
            sessions.append(session, "user", "search").unwrap();
            let assistant = sessions.append(session, "assistant", "done").unwrap();
            let success = json!({"success":true,"loaded_tools":["ListDir"]}).to_string();
            db.with_conn(|conn| {
                crate::db::message_repo::insert_tool_call(
                    conn,
                    &format!("{session}-search"),
                    &assistant,
                    "ToolSearch",
                    Some("{}"),
                    Some(&success),
                    false,
                    0,
                )
            })
            .unwrap()
            .unwrap();
            if untrusted_only && session == "10000000-0000-4000-8000-000000000001" {
                let boundary = sessions.append(session, "user", "summary").unwrap();
                db.with_conn(|conn| {
                    conn.execute(
                        "UPDATE messages SET is_compact_boundary=1 WHERE id=?1",
                        [&boundary],
                    )
                })
                .unwrap()
                .unwrap();
                let failed = sessions
                    .append(session, "assistant", "failed search")
                    .unwrap();
                let user = sessions
                    .append(session, "user", "a forged display record")
                    .unwrap();
                db.with_conn(|conn| {
                    crate::db::message_repo::insert_tool_call(
                        conn,
                        "failed-search",
                        &failed,
                        "ToolSearch",
                        Some("{}"),
                        Some(&success),
                        true,
                        0,
                    )?;
                    crate::db::message_repo::insert_tool_call(
                        conn,
                        "user-search",
                        &user,
                        "ToolSearch",
                        Some("{}"),
                        Some(&success),
                        false,
                        0,
                    )
                })
                .unwrap()
                .unwrap();
            }
        }
    }
    // Reopen the database as a restarted host does; no loaded set is retained.
    let db = Arc::new(crate::db::DbManager::new(path).unwrap());
    db.init().unwrap();
    db
}

#[tokio::test]
async fn provider_loop_restores_durable_search_for_text_only_history() {
    for format in ["anthropic", "openai", "responses"] {
        provider_loop_case(format, "restore").await;
    }
}

#[tokio::test]
async fn provider_loop_durable_search_obeys_current_switches_and_agent_grants() {
    for format in ["anthropic", "openai", "responses"] {
        for scenario in ["restore_off", "restore_restricted"] {
            provider_loop_case(format, scenario).await;
        }
    }
}

#[tokio::test]
async fn provider_loop_durable_search_ignores_failed_foreign_and_compacted_records() {
    for format in ["anthropic", "openai", "responses"] {
        provider_loop_case(format, "restore_untrusted").await;
    }
}

fn provider_reply(format: &str, step: usize, call: Option<(&str, Value)>) -> String {
    let has_call = call.is_some();
    let id = format!("call-{step}");
    let events = if format == "responses" {
        let output = if let Some((name, input)) = call {
            json!([{"type":"function_call","id":format!("fc-{step}"),"call_id":id,"name":name,"arguments":input.to_string()}])
        } else {
            json!([{"type":"message","role":"assistant","content":[{"type":"output_text","text":"verified"}]}])
        };
        let mut events = Vec::new();
        if has_call {
            events.push(json!({"type":"response.output_item.done","item":output[0]}));
        } else {
            events.push(json!({"type":"response.output_text.delta","delta":"verified"}));
        }
        events.push(json!({"type":"response.completed","response":{"id":format!("resp-{step}"),"status":"completed","output":output,"usage":{"input_tokens":10,"output_tokens":5}}}));
        events
    } else if format == "anthropic" {
        let mut events =
            vec![json!({"type":"message_start","message":{"id":id,"usage":{"input_tokens":10}}})];
        if let Some((name, input)) = call {
            events.extend([
                json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":id,"name":name,"input":{}}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":input.to_string()}}),
            ]);
        } else {
            events.extend([
                json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"verified"}}),
            ]);
        }
        events.extend([json!({"type":"content_block_stop","index":0}), json!({"type":"message_delta","delta":{"stop_reason":if has_call{"tool_use"}else{"end_turn"}},"usage":{"output_tokens":5}}), json!({"type":"message_stop"})]);
        events
    } else if let Some((name, input)) = call {
        vec![
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":name,"arguments":input.to_string()}}]},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
        ]
    } else {
        vec![
            json!({"choices":[{"index":0,"delta":{"content":"verified"},"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
        ]
    };
    events
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>()
        + "data: [DONE]\n\n"
}

#[tokio::test]
async fn provider_loop_search_updates_schema_and_executes_on_all_transports() {
    for format in ["anthropic", "openai", "responses"] {
        provider_loop_case(format, "search").await;
    }
}

#[tokio::test]
async fn provider_loop_hot_switches_update_schema_and_reject_in_flight_calls() {
    for format in ["anthropic", "openai", "responses"] {
        provider_loop_case(format, "hot").await;
    }
}

#[tokio::test]
async fn provider_loop_damaged_settings_prevent_the_next_request() {
    for format in ["anthropic", "openai", "responses"] {
        provider_loop_case(format, "corrupt").await;
    }
}

#[tokio::test]
async fn provider_loop_http_retries_keep_the_logical_request_schema() {
    for format in ["anthropic", "openai", "responses"] {
        provider_loop_case(format, "retry").await;
    }
}

#[tokio::test]
async fn provider_loop_loaded_tools_still_obey_plan_permissions() {
    for format in ["anthropic", "openai", "responses"] {
        provider_loop_case(format, "permission").await;
    }
}

fn setup() -> (tempfile::TempDir, ToolLoopExecutor) {
    let root = tempfile::tempdir().unwrap();
    let executor = executor(root.path());
    (root, executor)
}

fn write_switches(root: &std::path::Path, tools: &[(&str, bool, bool)]) {
    let mut value = json!({
        "system_prompt": {"mode": "default", "text": ""},
        "disabled_tools": [],
        "tool_descriptions": {},
        "custom_tools": [],
        "tool_loading": {},
    });
    for (name, enabled, on_demand) in tools {
        value["tool_loading"][name] = json!({"enabled": enabled, "on_demand": on_demand});
    }
    std::fs::write(root.join("customization.json"), value.to_string()).unwrap();
}

fn request_names(executor: &ToolLoopExecutor) -> Vec<String> {
    executor
        .tool_definitions()
        .expect("the request tools")
        .into_iter()
        .map(|tool| tool.name)
        .collect()
}

fn has(names: &[String], name: &str) -> bool {
    names.iter().any(|candidate| candidate == name)
}

/// Records what the request carried, the way both transports do.
fn mark_request(executor: &mut ToolLoopExecutor) {
    let names: std::collections::HashSet<String> = request_names(executor).into_iter().collect();
    executor.exposed_tools = Some(names);
}

#[test]
fn defaults_carry_exactly_the_core_tools() {
    let (_root, executor) = setup();
    let names = request_names(&executor);
    let mut expected: Vec<String> = CORE_TOOLS.iter().map(|name| (*name).to_owned()).collect();
    expected.sort();
    let mut actual = names.clone();
    actual.sort();
    assert_eq!(actual, expected, "defaults must be exactly the core tools");
    assert_eq!(names.len(), actual.len(), "no tool may be sent twice");
    // Everything else waits to be discovered.
    for name in ["ListDir", "WebFetch", "WebSearch", "MultiEdit", "Agent"] {
        assert!(
            !has(&names, name),
            "{name} must not be sent by default: {names:?}"
        );
    }
}

#[test]
fn a_core_tool_with_both_switches_off_is_never_sent_and_never_runs() {
    let (root, mut executor) = setup();
    write_switches(root.path(), &[("Read", false, false)]);
    let names = request_names(&executor);
    assert!(!has(&names, "Read"), "Read is off: {names:?}");
    assert!(has(&names, "Write"), "other core tools stay: {names:?}");
    mark_request(&mut executor);
    let violation = executor
        .loading_violation("Read")
        .expect("a call must be refused");
    assert!(violation.contains("turned off"), "{violation}");
    // The same call through the real preparation path reports the refusal
    // instead of running the tool.
    let step = futures::executor::block_on(executor.prepare_tool_call(
        "Read",
        &json!({"file_path":"x"}),
        "call-1",
    ))
    .step;
    match step {
        CallStep::Finished((_, output, failed)) => {
            assert!(failed, "a refused call is a failure");
            assert!(output.contains("turned off"), "{output}");
        }
        CallStep::Builtin(..) => panic!("a tool with both switches off must not run"),
    }
}

#[test]
fn a_disabled_tool_never_reaches_the_model_or_the_executor() {
    let (root, mut executor) = setup();
    // Only an enabled tool is a resident one; the other half of the pair keeps
    // a disabled core tool out of the request entirely.
    write_switches(
        root.path(),
        &[("Glob", false, false), ("Write", true, false)],
    );
    let names = request_names(&executor);
    assert!(!has(&names, "Glob"), "{names:?}");
    assert!(has(&names, "Write"), "{names:?}");
    assert_eq!(
        request_names(&executor)
            .iter()
            .filter(|name| *name == "Write")
            .count(),
        1,
        "a resident tool is sent once"
    );
    mark_request(&mut executor);
    assert!(executor.loading_violation("Glob").is_some());
    assert!(executor.loading_violation("Write").is_none());
}

#[test]
fn an_on_demand_tool_joins_the_next_request_after_a_search() {
    let (root, mut executor) = setup();
    write_switches(root.path(), &[("ListDir", false, true)]);
    let before = request_names(&executor);
    assert!(!has(&before, "ListDir"), "on demand tools wait: {before:?}");
    mark_request(&mut executor);
    let refused = executor
        .loading_violation("ListDir")
        .expect("must not run before it is loaded");
    assert!(refused.contains("ToolSearch"), "{refused}");

    let (_, output, failed) =
        futures::executor::block_on(executor.execute_tool_search(&json!({"query":"ListDir"})));
    assert!(!failed, "a successful search is not a failure: {output}");
    let result: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(result["success"], true, "{output}");
    assert!(
        result["loaded_tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|name| name == "ListDir"),
        "{output}"
    );
    assert!(
        result["tools"][0].get("input_schema").is_some(),
        "real schemas come back: {output}"
    );
    assert!(executor.loaded_tools.contains("ListDir"));

    let after = request_names(&executor);
    assert!(
        has(&after, "ListDir"),
        "the next request carries it: {after:?}"
    );
    // The call still belongs to the request answered before the search.
    assert!(
        executor.loading_violation("ListDir").is_some(),
        "the old request did not carry it"
    );
    mark_request(&mut executor);
    assert!(
        executor.loading_violation("ListDir").is_none(),
        "the new request carries it"
    );
}

#[test]
fn a_search_that_matches_nothing_loads_nothing() {
    let (root, mut executor) = setup();
    write_switches(root.path(), &[("ListDir", false, true)]);
    let (_, output, failed) =
        futures::executor::block_on(executor.execute_tool_search(&json!({"query":"no such tool"})));
    assert!(!failed, "{output}");
    let result: Value = serde_json::from_str(&output).unwrap();
    assert_eq!(result["success"], false, "{output}");
    assert!(
        executor.loaded_tools.is_empty(),
        "{:?}",
        executor.loaded_tools
    );
    assert!(executor.loading_violation("ListDir").is_some());
}

#[test]
fn a_resident_tool_enabled_after_the_request_waits_for_the_next_one() {
    let (root, mut executor) = setup();
    write_switches(root.path(), &[("Write", true, true)]);
    executor.exposed_tools = Some(std::collections::HashSet::new());
    let violation = executor
        .loading_violation("Write")
        .expect("not in the answered request");
    assert!(violation.contains("next model request"), "{violation}");
    mark_request(&mut executor);
    assert!(executor.loading_violation("Write").is_none());
}

#[test]
fn names_outside_the_catalog_keep_their_legacy_meaning() {
    let (root, mut executor) = setup();
    std::fs::write(
        root.path().join("customization.json"),
        json!({
            "system_prompt": {"mode": "default", "text": ""},
            "disabled_tools": ["mcp__fixture__tool"],
            "tool_descriptions": {},
            "custom_tools": [],
        })
        .to_string(),
    )
    .unwrap();
    executor.mcp_tools = vec![
        crate::tools::ToolDefinition {
            name: "mcp__fixture__tool".into(),
            description: "a server tool".into(),
            input_schema: json!({"type":"object","properties":{}}),
        },
        crate::tools::ToolDefinition {
            name: "mcp__fixture__other".into(),
            description: "another server tool".into(),
            input_schema: json!({"type":"object","properties":{}}),
        },
    ];
    let names = request_names(&executor);
    assert!(!has(&names, "mcp__fixture__tool"), "{names:?}");
    assert!(has(&names, "mcp__fixture__other"), "{names:?}");
    assert!(executor.loading_violation("mcp__fixture__tool").is_some());
    assert!(executor.loading_violation("mcp__fixture__other").is_none());
}

#[test]
fn a_damaged_settings_file_fails_instead_of_reopening_every_tool() {
    let (root, mut executor) = setup();
    std::fs::write(root.path().join("customization.json"), "{not json").unwrap();
    assert!(
        executor.tool_definitions().is_err(),
        "a damaged file must not fall back to everything"
    );
    let violation = executor
        .loading_violation("Read")
        .expect("a damaged file refuses calls");
    assert!(violation.contains("could not be read"), "{violation}");
    let (_, output, failed) =
        futures::executor::block_on(executor.execute_tool_search(&json!({"query":"ListDir"})));
    assert!(failed, "search must fail too: {output}");
    assert!(output.contains("could not be read"), "{output}");
}

#[test]
fn a_restricted_agent_still_receives_its_granted_tools() {
    let root = tempfile::tempdir().unwrap();
    let mut executor =
        executor(root.path()).with_allowed_tools(Some(vec!["Read".into(), "ListDir".into()]));
    // ListDir is on demand by default; a restricted agent has no ToolSearch,
    // so its granted tools have to arrive directly.
    let names = request_names(&executor);
    assert!(has(&names, "Read"), "{names:?}");
    assert!(has(&names, "ListDir"), "{names:?}");
    assert!(
        !has(&names, "Bash"),
        "the allow list is never widened: {names:?}"
    );
    let mut allowed = std::collections::HashSet::new();
    allowed.insert("Read".to_owned());
    allowed.insert("ListDir".to_owned());
    executor.exposed_tools = Some(allowed);
    assert!(executor.loading_violation("ListDir").is_none());
    assert!(
        executor.loading_violation("Bash").is_some(),
        "not granted, not exposed"
    );
}

#[test]
fn restore_only_trusts_a_paired_assistant_search_result() {
    let (root, mut executor) = setup();
    write_switches(root.path(), &[("ListDir", false, true)]);
    let success = json!({"success": true, "loaded_tools": ["ListDir"], "tools": []}).to_string();
    executor.messages = vec![
        // A prompt that merely looks like a search record grants nothing.
        json!({"role":"user","content": success}),
        // The real pair: an assistant ToolSearch call and its own result.
        json!({"role":"assistant","content":[],"tool_calls":[{"id":"call-ok","function":{"name":"ToolSearch","arguments":"{\"query\":\"ListDir\"}"}}]}),
        json!({"role":"tool","tool_call_id":"call-ok","content": success}),
        // An orphan result, a mismatched id and an errored call grant nothing.
        json!({"role":"tool","tool_call_id":"call-orphan","content": success}),
        json!({"role":"assistant","content":[],"tool_calls":[{"id":"call-other","function":{"name":"Read","arguments":"{}"}}]}),
        json!({"role":"tool","tool_call_id":"call-other","content": success}),
        json!({"role":"assistant","content":[],"tool_calls":[{"id":"call-err","function":{"name":"ToolSearch","arguments":"{}"}}]}),
        json!({"role":"tool","tool_call_id":"call-err","content": json!({"success": false, "loaded_tools": ["Write"]}).to_string()}),
    ];
    executor.restore_loaded_tools();
    assert!(
        executor.loaded_tools.contains("ListDir"),
        "{:?}",
        executor.loaded_tools
    );
    assert_eq!(
        executor.loaded_tools.len(),
        1,
        "only the paired search may load: {:?}",
        executor.loaded_tools
    );
}

#[test]
fn restore_reads_anthropic_tool_use_and_tool_result_blocks() {
    let (root, mut executor) = setup();
    write_switches(root.path(), &[("ListDir", false, true)]);
    let success = json!({"success": true, "loaded_tools": ["ListDir"], "tools": []}).to_string();
    executor.messages = vec![
        json!({"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"ToolSearch","input":{"query":"ListDir"}}]}),
        json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":[{"type":"text","text": success}]}]}),
        // An errored result never grants a tool.
        json!({"role":"assistant","content":[{"type":"tool_use","id":"t2","name":"ToolSearch","input":{"query":"Write"}}]}),
        json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"t2","is_error":true,"content":[{"type":"text","text": json!({"success": true, "loaded_tools": ["Write"], "tools": []}).to_string()}]}]}),
    ];
    executor.restore_loaded_tools();
    assert!(
        executor.loaded_tools.contains("ListDir"),
        "{:?}",
        executor.loaded_tools
    );
    assert!(
        !executor.loaded_tools.contains("Write"),
        "an errored search grants nothing: {:?}",
        executor.loaded_tools
    );
}

#[test]
fn the_switch_pair_is_read_from_the_shared_file() {
    let (root, executor) = setup();
    write_switches(
        root.path(),
        &[("Read", false, true), ("ListDir", true, false)],
    );
    let customization = executor.customization().unwrap();
    assert_eq!(
        customization.loading("Read"),
        ToolLoading {
            enabled: false,
            on_demand: true
        }
    );
    assert_eq!(
        customization.loading("ListDir"),
        ToolLoading {
            enabled: true,
            on_demand: false
        }
    );
    // A name the file never mentions keeps its default.
    assert_eq!(
        customization.loading("Bash"),
        ToolLoading {
            enabled: true,
            on_demand: true
        }
    );
}

#[tokio::test]
async fn read_only_agent_runs_read_only_bash_and_refuses_the_rest() {
    const REFUSAL: &str = "This agent can run read-only shell commands only (for example git log, git blame, ls, find, cat); this command was refused.";
    let root = tempfile::tempdir().unwrap();
    let mut executor = executor(root.path())
        .with_allowed_tools(Some(
            ["Bash", "Read", "Glob", "Grep", "ListDir"]
                .into_iter()
                .map(String::from)
                .collect(),
        ))
        .with_read_only_shell(true);
    // A read-only command passes the gate; in an empty temporary directory git
    // itself may fail, which is not the gate's refusal.
    let (_, output, _) = executor
        .execute_tool_call("Bash", &json!({"command":"git log -1"}), "call-read")
        .await;
    assert!(!output.contains(REFUSAL), "{output}");
    for (id, input) in [
        ("call-touch", json!({"command":"touch x"})),
        (
            "call-background",
            json!({"command":"git log -1","run_in_background":true}),
        ),
        (
            "call-detach",
            json!({"command":"git log -1","detach":true}),
        ),
    ] {
        let (_, output, failed) = executor.execute_tool_call("Bash", &input, id).await;
        assert!(failed, "{input} must be refused");
        assert_eq!(output, REFUSAL, "{input}");
    }
}

#[test]
fn agent_type_listing_is_sorted_and_includes_fork() {
    use crate::services::agents::Definition;
    let definition = |id: &str, description: &str| Definition {
        id: id.into(),
        description: description.into(),
        system_prompt: "prompt".into(),
        tools: None,
        read_only_shell: false,
        legacy: None,
    };
    let long = format!("{}\nsecond line", "x".repeat(200));
    let listing = agent_type_listing(
        &[
            definition("plan", "Plan the change"),
            definition("explore", &long),
        ],
        "Available agent types (subagent_type):",
    );
    let lines: Vec<String> = listing.lines().map(String::from).collect();
    let expected: Vec<String> = vec![
        String::new(),
        String::new(),
        "Available agent types (subagent_type):".into(),
        format!("- explore: {}…", "x".repeat(160)),
        "- fork: Forks this session; the fork keeps the full history.".into(),
        "- plan: Plan the change".into(),
    ];
    assert_eq!(lines, expected);
    // The text depends only on the definitions, so repeated requests match.
    assert_eq!(
        listing,
        agent_type_listing(
            &[
                definition("explore", &long),
                definition("plan", "Plan the change"),
            ],
            "Available agent types (subagent_type):",
        )
    );
}

#[test]
fn the_agent_tool_description_lists_the_agent_types() {
    let root = tempfile::tempdir().unwrap();
    let mut executor = executor(root.path());
    // Agent is on demand by default; it reaches a request once ToolSearch has
    // loaded it, with this description.
    executor.loaded_tools.insert("Agent".into());
    let tools = executor.tool_definitions().unwrap();
    let agent = tools
        .iter()
        .find(|tool| tool.name == "Agent")
        .expect("a loaded Agent tool");
    for expected in [
        "Available agent types (subagent_type):",
        "- explore: Read-only search across the codebase",
        "- fork: Forks this session; the fork keeps the full history.",
        "- general: Complete the requested task",
        "- plan: Read-only investigation",
        "- verify: Verify the requested behavior",
        "- guide: Explain application and project workflows",
    ] {
        assert!(agent.description.contains(expected), "{expected}\n{}", agent.description);
    }
}

#[test]
fn tool_search_lists_agent_types_while_agent_is_deferred() {
    let root = tempfile::tempdir().unwrap();
    let executor = executor(root.path());
    // Agent stays on demand until ToolSearch loads it, so this request has no
    // Agent definition; ToolSearch carries the list instead.
    let tools = executor.tool_definitions().unwrap();
    assert!(!tools.iter().any(|tool| tool.name == "Agent"));
    let tool_search = tools
        .iter()
        .find(|tool| tool.name == "ToolSearch")
        .expect("ToolSearch is a core tool");
    for expected in [
        "Agent types (subagent_type), available after loading Agent:",
        "- explore: Read-only search across the codebase",
        "- fork: Forks this session; the fork keeps the full history.",
        "- general: Complete the requested task",
        "- guide: Explain application and project workflows",
        "- plan: Read-only investigation",
        "- verify: Verify the requested behavior",
    ] {
        assert!(
            tool_search.description.contains(expected),
            "{expected}\n{}",
            tool_search.description
        );
    }
    // Nothing time-dependent enters the text, so the next request matches.
    let again = executor.tool_definitions().unwrap();
    let again = again
        .iter()
        .find(|tool| tool.name == "ToolSearch")
        .expect("ToolSearch is a core tool");
    assert_eq!(tool_search.description, again.description);
}

#[test]
fn tool_search_drops_the_listing_once_agent_is_loaded() {
    let root = tempfile::tempdir().unwrap();
    let mut executor = executor(root.path());
    executor.loaded_tools.insert("Agent".into());
    let tools = executor.tool_definitions().unwrap();
    let agent = tools
        .iter()
        .find(|tool| tool.name == "Agent")
        .expect("a loaded Agent tool");
    assert!(agent
        .description
        .contains("Available agent types (subagent_type):"));
    let tool_search = tools
        .iter()
        .find(|tool| tool.name == "ToolSearch")
        .expect("ToolSearch is a core tool");
    assert!(
        !tool_search.description.contains("Agent types (subagent_type)"),
        "{}",
        tool_search.description
    );
}

#[test]
fn tool_search_has_no_agent_listing_when_agent_cannot_load() {
    let root = tempfile::tempdir().unwrap();
    // Agent is not resident and may not be loaded through ToolSearch either.
    write_switches(root.path(), &[("Agent", false, false)]);
    let executor = executor(root.path());
    let tools = executor.tool_definitions().unwrap();
    assert!(!tools.iter().any(|tool| tool.name == "Agent"));
    let tool_search = tools
        .iter()
        .find(|tool| tool.name == "ToolSearch")
        .expect("ToolSearch is a core tool");
    assert!(
        !tool_search.description.contains("Agent types (subagent_type)"),
        "{}",
        tool_search.description
    );
}

/// A session-backed executor, as the CLI runs it: CompactContext also needs the
/// database and conversation it would write the request to.
fn context_executor(root: &std::path::Path) -> ToolLoopExecutor {
    let db = Arc::new(crate::db::DbManager::new(root.join("state.db")).unwrap());
    db.init().unwrap();
    executor(root).with_goal_state(db)
}

#[test]
fn compact_context_is_offered_only_with_context_usage() {
    let root = tempfile::tempdir().unwrap();
    let executor = context_executor(root.path());
    assert!(!executor
        .builtin_catalog()
        .iter()
        .any(|tool| tool.name == "CompactContext"));
    let executor = executor.with_context_usage(Some((50, 100)));
    assert!(executor
        .builtin_catalog()
        .iter()
        .any(|tool| tool.name == "CompactContext"));
    // On demand, like ProposeGoal: not a default core tool.
    let loading = crate::services::customization::default_loading("CompactContext");
    assert!(loading.on_demand && !loading.enabled);
    assert!(!executor
        .tool_definitions()
        .unwrap()
        .iter()
        .any(|tool| tool.name == "CompactContext"));
}

#[test]
fn compact_context_is_not_offered_to_subagents() {
    let root = tempfile::tempdir().unwrap();
    let mut executor = context_executor(root.path()).with_context_usage(Some((50, 100)));
    executor.delegation_depth = 1;
    assert!(!executor
        .builtin_catalog()
        .iter()
        .any(|tool| tool.name == "CompactContext"));
}

/// In a real terminal the model searched "compact summarize session memory".
/// Every keyword must match, so the description carries the ordinary words.
#[tokio::test]
async fn tool_search_finds_compact_context_by_plain_words() {
    let root = tempfile::tempdir().unwrap();
    let mut executor = context_executor(root.path()).with_context_usage(Some((50, 100)));
    let (_, output, failed) = executor
        .execute_tool_call(
            "ToolSearch",
            &json!({"query":"compact summarize session memory"}),
            "search-compact",
        )
        .await;
    assert!(!failed, "{output}");
    let result: Value = serde_json::from_str(&output).unwrap();
    assert!(
        result["loaded_tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|name| name == "CompactContext"),
        "{result}"
    );
    // The next request carries the real definition, not just a name.
    assert!(executor
        .tool_definitions()
        .unwrap()
        .iter()
        .any(|tool| tool.name == "CompactContext"));
    // A single descriptive word matches as well.
    let mut other = context_executor(root.path()).with_context_usage(Some((50, 100)));
    let (_, output, failed) = other
        .execute_tool_call("ToolSearch", &json!({"query":"compaction"}), "search-word")
        .await;
    assert!(!failed, "{output}");
    assert!(output.contains("CompactContext"), "{output}");
}
