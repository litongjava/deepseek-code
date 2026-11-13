use super::*;
use crate::{
    db::conversation_repo,
    native_engine::{
        engine_core::ReasoningMode, provider_manager::ResolvedProvider,
        runtime_agents::RuntimeAgent,
    },
    services::{agents, EnvironmentCredentials},
};
use axum::{extract::State, routing::post, Json, Router};
use std::sync::{Arc, Mutex};

async fn executor(
    root: &std::path::Path,
    parent: &str,
    endpoint: String,
    _requests: Arc<Mutex<Vec<Value>>>,
) -> (ToolLoopExecutor, ServiceContext) {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let context = ServiceContext::open(
        RuntimePaths::new(root.join("data")).unwrap(),
        Arc::new(EnvironmentCredentials),
    )
    .unwrap();
    context
        .db
        .with_conn(|conn| -> Result<()> {
            conversation_repo::insert_conversation(
                conn,
                parent,
                None,
                Some("fixture"),
                Some("fixture"),
                root.to_str(),
                None,
                false,
                false,
                false,
                "now",
                "now",
                0,
            )?;
            Ok(())
        })
        .unwrap()
        .unwrap();
    let private_root = root.join("private-root");
    let cwd = context
        .db
        .with_conn(|conn| {
            crate::native_engine::workspace::resolve_workspace(conn, &private_root, parent, None)
        })
        .unwrap()
        .unwrap()
        .0;
    std::fs::create_dir_all(&cwd).unwrap();
    let provider = ResolvedProvider {
        provider: serde_json::from_value(json!({"id":"fixture","name":"fixture","base_url":endpoint,"api_key":"unused","api_format":"openai","models":[],"enabled":true})).unwrap(),
        model: serde_json::from_value(json!({"id":"fixture","name":"fixture","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap(),
    };
    let (events, _receiver) = tokio::sync::mpsc::channel(128);
    let executor = ToolLoopExecutor::new(
        provider,
        vec![],
        None,
        128,
        events,
        cwd.to_string_lossy().into_owned(),
        ReasoningMode::Standard,
    )
    .with_conv_id(parent.to_string())
    .with_search_config_path(context.paths.data_dir.join("settings.json"))
    .with_runtime_agents(Arc::new(vec![RuntimeAgent {
        definition: agents::Definition {
            id: "analyst".into(),
            description: "fixture analyst".into(),
            system_prompt: "AGENT_PROMPT_SENTINEL".into(),
            tools: None,
            read_only_shell: false,
            legacy: None,
        },
        provider: None,
        disallowed_tools: Vec::new(),
        max_turns: Some(2),
        mcp_servers: Vec::new(),
        skill_context: Some("# Preloaded skill\nSKILL_SENTINEL".into()),
        memory: Some("user".into()),
        permission_mode: Some("accept_edits".into()),
        effort: Some("high".into()),
        initial_prompt: None,
        reminder: Some("REMINDER_SENTINEL".into()),
    }]));
    (executor, context)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_agent_fields_reach_the_child_request() {
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let app = Router::new()
        .route(
            "/v1/chat/completions",
            post(
                |State(requests): State<Arc<Mutex<Vec<Value>>>>, Json(body): Json<Value>| async move {
                    requests.lock().unwrap().push(body);
                    let stream = format!(
                        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                        json!({"choices":[{"index":0,"delta":{"content":"done"},"finish_reason":null}]}),
                        json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})
                    );
                    ([("content-type", "text/event-stream")], stream)
                },
            ),
        )
        .with_state(requests.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let temp = tempfile::tempdir().unwrap();
    let parent = uuid::Uuid::new_v4().to_string();
    let (executor, context) = executor(temp.path(), &parent, endpoint, requests.clone()).await;
    std::fs::create_dir_all(context.paths.data_dir.join("agent-memory/analyst")).unwrap();
    std::fs::write(
        context.paths.data_dir.join("agent-memory/analyst/notes.md"),
        "MEMORY_SENTINEL",
    )
    .unwrap();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        executor.execute_delegated(&json!({"prompt":"go","agent_type":"analyst"})),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result["status"], "completed");
    let bodies = requests.lock().unwrap();
    let body = bodies.first().expect("request");
    let system = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| message["content"].as_str().unwrap_or(""))
        .collect::<Vec<_>>()
        .join("\n");
    for sentinel in [
        "AGENT_PROMPT_SENTINEL",
        "SKILL_SENTINEL",
        "MEMORY_SENTINEL",
        "REMINDER_SENTINEL",
    ] {
        assert!(system.contains(sentinel), "missing {sentinel}: {system}");
    }
    assert_eq!(body["reasoning_effort"], "high");
    drop(bodies);
    server.abort();
}
