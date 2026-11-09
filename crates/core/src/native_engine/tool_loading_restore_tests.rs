//! A session that searched a tool before it ended must find it again after the
//! session is reopened, and no other session may.

use super::*;
use crate::runtime::{RuntimePaths, SessionStore};

fn restore_executor(paths: &RuntimePaths, messages: Vec<Value>) -> ToolLoopExecutor {
    let _ = crate::config::OrchestrationConfig::init_global();
    let (tx, _rx) = mpsc::channel(64);
    ToolLoopExecutor::new(
        ResolvedProvider {
            provider: serde_json::from_value(json!({"id":"fixture","name":"fixture","base_url":"http://127.0.0.1:1","api_key":"unused","api_format":"openai","models":[],"enabled":true})).unwrap(),
            model: serde_json::from_value(json!({"id":"fixture","name":"fixture","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap(),
        },
        messages,
        None,
        256,
        tx,
        paths.data_dir.to_string_lossy().into_owned(),
        ReasoningMode::Standard,
    )
    .with_search_config_path(paths.data_dir.join("config.json"))
}

/// Stores one assistant tool call and its result the way the host does.
fn record_call(
    store: &SessionStore,
    session: &str,
    assistant: &str,
    id: &str,
    name: &str,
    output: &str,
) {
    store
        .db
        .with_conn(|conn| {
            crate::db::message_repo::save_stream_tool_result(
                conn,
                session,
                assistant,
                "",
                &crate::db::message_repo::ToolCallRow {
                    id: format!("{assistant}:{id}"),
                    message_id: assistant.to_owned(),
                    name: name.to_owned(),
                    input: Some("{}".to_owned()),
                    output: Some(output.to_owned()),
                    is_error: false,
                    sort_order: 0,
                },
            )
        })
        .unwrap()
        .unwrap();
}

fn request_names(executor: &ToolLoopExecutor) -> Vec<String> {
    executor
        .tool_definitions()
        .unwrap()
        .into_iter()
        .map(|tool| tool.name)
        .collect()
}

#[test]
fn a_persisted_search_still_counts_after_a_restart_and_only_in_that_session() {
    let _ = crate::config::OrchestrationConfig::init_global();
    let dir = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(dir.path().join("data")).unwrap();
    let store = SessionStore::new(paths.open_database().unwrap());
    let session = store.create(dir.path(), "fixture", "fixture").unwrap();
    let marker = json!({"success":true,"loaded_tools":["ListDir"],"tools":[]}).to_string();
    let assistant = uuid::Uuid::new_v4().to_string();

    store
        .append(&session, "user", "List the fixture files")
        .unwrap();
    store.append(&session, "assistant", "").unwrap();
    record_call(
        &store,
        &session,
        &assistant,
        "search-1",
        "ToolSearch",
        &marker,
    );
    store.append(&session, "user", "Continue").unwrap();

    // Reopening the session restores the load from the stored call and result.
    let mut resumed = restore_executor(
        &paths,
        store.model_history(&session, &ApiFormat::OpenAI).unwrap(),
    );
    resumed.restore_loaded_tools();
    assert!(request_names(&resumed).contains(&"ListDir".to_owned()));

    // The Anthropic history shape restores it the same way.
    let mut anthropic = restore_executor(
        &paths,
        store
            .model_history(&session, &ApiFormat::Anthropic)
            .unwrap(),
    );
    anthropic.restore_loaded_tools();
    assert!(request_names(&anthropic).contains(&"ListDir".to_owned()));

    // A different session shares nothing with it.
    let other = store.create(dir.path(), "fixture", "fixture").unwrap();
    store
        .append(&other, "user", "List the fixture files")
        .unwrap();
    let mut fresh = restore_executor(
        &paths,
        store.model_history(&other, &ApiFormat::OpenAI).unwrap(),
    );
    fresh.restore_loaded_tools();
    assert!(!request_names(&fresh).contains(&"ListDir".to_owned()));

    // A failed search in the past grants nothing.
    let failed_session = store.create(dir.path(), "fixture", "fixture").unwrap();
    let failed_assistant = uuid::Uuid::new_v4().to_string();
    store.append(&failed_session, "user", "List").unwrap();
    store.append(&failed_session, "assistant", "").unwrap();
    record_call(
        &store,
        &failed_session,
        &failed_assistant,
        "search-2",
        "ToolSearch",
        &json!({"success":false,"loaded_tools":[],"tools":[]}).to_string(),
    );
    let mut rejected = restore_executor(
        &paths,
        store
            .model_history(&failed_session, &ApiFormat::OpenAI)
            .unwrap(),
    );
    rejected.restore_loaded_tools();
    assert!(!request_names(&rejected).contains(&"ListDir".to_owned()));

    // Compression can drop the record; the search then simply runs again.
    let summarized: Vec<Value> = store
        .model_history(&session, &ApiFormat::OpenAI)
        .unwrap()
        .into_iter()
        .filter(|message| message["role"] != "tool")
        .collect();
    let mut compacted = restore_executor(&paths, summarized);
    compacted.restore_loaded_tools();
    assert!(!request_names(&compacted).contains(&"ListDir".to_owned()));
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let (_, output, failed) = compacted
            .execute_tool_call("ToolSearch", &json!({"query":"ListDir"}), "search-3")
            .await;
        assert!(!failed, "{output}");
        let value: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(value["loaded_tools"], json!(["ListDir"]));
        // The stored result carries the real schema, not a placeholder.
        assert!(value["tools"][0]["input_schema"].is_object(), "{output}");
        // And it becomes callable from the next request on.
        compacted.exposed_tools = Some(request_names(&compacted).into_iter().collect());
        let (_, output, failed) = compacted
            .execute_tool_call("ListDir", &json!({"path":"."}), "list")
            .await;
        assert!(!failed, "{output}");
    });
}
