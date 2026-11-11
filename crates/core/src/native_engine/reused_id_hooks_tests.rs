#![cfg(unix)]
//! Repeated provider raw ids must still run every hook and every permission
//! check. A raw id is provider provenance, never an execution key.
use super::*;
use crate::services::tool_hooks::HookRunner;

fn provider(base: &str) -> ResolvedProvider {
    crate::config::OrchestrationConfig::init_global().unwrap();
    ResolvedProvider {
        provider: serde_json::from_value(json!({
            "id": "reused-hook-fixture", "name": "reused-hook-fixture",
            "base_url": base, "api_key": "fixture-only", "api_format": "openai",
            "enabled": true, "models": []
        }))
        .unwrap(),
        model: serde_json::from_value(json!({
            "id": "fixture", "name": "fixture", "context_window": 8000,
            "enabled": true, "supports_web_search": false
        }))
        .unwrap(),
    }
}

fn hook(command: &str) -> Value {
    json!({"type":"command","shell":"sh","command":command})
}

fn json_lines(path: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[derive(Debug, Clone)]
struct Done {
    name: String,
    id: String,
    input: Value,
    output: String,
    is_error: bool,
}

#[tokio::test]
async fn repeated_raw_ids_fire_each_hook_and_permission_request() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    // PreToolUse rewrites only the first command and records every call.
    std::fs::write(
        root.path().join("pre.sh"),
        r#"payload=$(cat)
printf '%s\n' "$payload" >> pre.jsonl
case "$payload" in
  *ORIGINAL*)
    printf '%s' '{"hookSpecificOutput":{"hookEventName":"PreToolUse","updatedInput":{"command":"printf HOOK_MODIFIED"}}}'
    ;;
esac
"#,
    )
    .unwrap();
    // PermissionRequest denies only the second call and records every request.
    std::fs::write(
        root.path().join("permission.sh"),
        r#"payload=$(cat)
printf '%s\n' "$payload" >> permission.jsonl
case "$payload" in
  *DENY_ME*)
    printf '%s' '{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny","message":"FIXTURE_DENIED"}}}'
    ;;
  *)
    printf '%s' '{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}'
    ;;
esac
"#,
    )
    .unwrap();
    std::fs::write(
        root.path().join("failure.sh"),
        "cat >> failure.jsonl\nprintf '\\n' >> failure.jsonl\n",
    )
    .unwrap();
    std::fs::write(
        data.join("settings.json"),
        json!({"hooks":{
            "PreToolUse":[{"matcher":"^Bash$","hooks":[hook("sh pre.sh")]}],
            "PermissionRequest":[{"matcher":"^Bash$","hooks":[hook("sh permission.sh")]}],
            "PostToolUseFailure":[{"matcher":"^Bash$","hooks":[hook("sh failure.sh")]}]
        }})
        .to_string(),
    )
    .unwrap();

    let commands = ["printf 'ORIGINAL'", "printf 'DENY_ME'", "exit 7"];
    let requests = Arc::new(std::sync::Mutex::new(0usize));
    let counter = requests.clone();
    let app = axum::Router::new().fallback(axum::routing::post(
        move |axum::Json(_body): axum::Json<Value>| {
            let index = {
                let mut counter = counter.lock().unwrap();
                let index = *counter;
                *counter += 1;
                index
            };
            async move {
                let frames = match commands.get(index) {
                    Some(command) => vec![
                        json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"same","type":"function","function":{"name":"Bash","arguments":json!({"command":command}).to_string()}}]},"finish_reason":null}]}),
                        json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
                    ],
                    None => vec![
                        json!({"choices":[{"index":0,"delta":{"content":"DONE"},"finish_reason":null}]}),
                        json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
                    ],
                };
                let mut stream = frames
                    .into_iter()
                    .map(|frame| format!("data: {frame}\n\n"))
                    .collect::<String>();
                stream.push_str("data: [DONE]\n\n");
                ([("content-type", "text/event-stream")], stream)
            }
        },
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let (tx, mut rx) = mpsc::channel(128);
    let manager = crate::runtime::default_permissions();
    manager.set_mode(crate::permissions::PermissionMode::AskPermissions);
    let mut executor = ToolLoopExecutor::new(
        provider(&base),
        vec![json!({"role":"user","content":"run the fixture commands"})],
        None,
        256,
        tx,
        root.path().to_string_lossy().into_owned(),
        ReasoningMode::Standard,
    )
    .with_conv_id("reused-hook-session".into())
    .with_permission_manager(manager)
    .with_permission_host(false)
    .with_hooks(HookRunner::load(&data, root.path()).unwrap());
    let result = tokio::time::timeout(std::time::Duration::from_secs(30), executor.execute())
        .await
        .unwrap()
        .unwrap();
    assert!(result.0.contains("DONE"), "{:?}", result.0);
    drop(executor);
    let mut dones = Vec::<Done>::new();
    while let Ok(event) = rx.try_recv() {
        if let EngineEvent::ToolUseDone {
            tool_use_id,
            tool_name,
            tool_input,
            output,
            is_error,
        } = event
        {
            dones.push(Done {
                name: tool_name,
                id: tool_use_id,
                input: tool_input,
                output,
                is_error,
            });
        }
    }
    server.abort();

    assert_eq!(dones.len(), 3, "{dones:?}");
    let ids: std::collections::HashSet<_> = dones.iter().map(|done| done.id.as_str()).collect();
    assert_eq!(ids.len(), 3, "each execution keeps its own identity");
    // The hook's changed input is the authoritative one that reaches the result.
    assert_eq!(dones[0].name, "Bash");
    assert_eq!(dones[0].input["command"], "printf HOOK_MODIFIED");
    assert!(dones[0].output.contains("HOOK_MODIFIED"), "{:?}", dones[0]);
    assert!(!dones[0].output.contains("ORIGINAL"), "{:?}", dones[0]);
    assert!(!dones[0].is_error);
    assert!(dones[1].is_error);
    assert!(
        dones[1].output.contains("FIXTURE_DENIED") || dones[1].output.contains("denied"),
        "{:?}",
        dones[1]
    );
    assert!(dones[2].is_error);

    // Hooks receive the unique occurrence id; the raw id "same" is provider
    // provenance only.
    let pre = json_lines(&root.path().join("pre.jsonl"));
    assert_eq!(pre.len(), 3, "{pre:?}");
    let pre_ids: Vec<&str> = pre
        .iter()
        .map(|event| event["tool_use_id"].as_str().unwrap())
        .collect();
    assert!(
        pre_ids.iter().all(|id| id.starts_with("inv-")),
        "{pre_ids:?}"
    );
    assert_eq!(
        pre_ids.iter().collect::<std::collections::HashSet<_>>().len(),
        3,
        "{pre_ids:?}"
    );
    let permission = json_lines(&root.path().join("permission.jsonl"));
    assert_eq!(permission.len(), 3, "{permission:?}");
    let permission_ids: Vec<&str> = permission
        .iter()
        .map(|event| event["tool_use_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        permission_ids, pre_ids,
        "PermissionRequest ids match PreToolUse ids in call order"
    );
    // The first call's permission request sees the hook-updated input.
    assert_eq!(
        permission[0]["tool_input"]["command"],
        "printf HOOK_MODIFIED"
    );
    assert_eq!(permission[1]["tool_input"]["command"], "printf 'DENY_ME'");
    // A denied call and a failed call are both failures, each reported against
    // one of the occurrence ids rather than the shared raw provider id.
    let failure = json_lines(&root.path().join("failure.jsonl"));
    assert_eq!(failure.len(), 2, "{failure:?}");
    let failure_ids: Vec<&str> = failure
        .iter()
        .map(|event| event["tool_use_id"].as_str().unwrap())
        .collect();
    assert!(
        failure_ids.iter().all(|id| pre_ids.contains(id)),
        "{failure_ids:?} is not a subset of {pre_ids:?}"
    );
    let failed_commands: std::collections::HashSet<_> = failure
        .iter()
        .map(|event| event["tool_input"]["command"].as_str().unwrap().to_owned())
        .collect();
    assert!(failed_commands.contains("printf 'DENY_ME'"), "{failure:?}");
    assert!(failed_commands.contains("exit 7"), "{failure:?}");

    // The changed input survives the durable write keyed by the occurrence id.
    let context = crate::services::session_messages::context(root.path()).unwrap();
    let store = crate::runtime::SessionStore::new(context.db.clone());
    let session = store.create(root.path(), "fixture", "fixture").unwrap();
    let assistant = store.append(&session, "assistant", "").unwrap();
    context
        .db
        .with_conn(|conn| -> anyhow::Result<()> {
            for (index, done) in dones.iter().enumerate() {
                crate::db::message_repo::insert_tool_call(
                    conn,
                    &format!("{assistant}:{}", done.id),
                    &assistant,
                    &done.name,
                    Some(&done.input.to_string()),
                    Some(&done.output),
                    done.is_error,
                    index as i64,
                )?;
            }
            Ok(())
        })
        .unwrap()
        .unwrap();
    let rows = context
        .db
        .with_conn(|conn| crate::db::message_repo::get_tool_calls_for_message(conn, &assistant))
        .unwrap()
        .unwrap();
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows[0].input.as_deref(),
        Some(r#"{"command":"printf HOOK_MODIFIED"}"#)
    );
}
