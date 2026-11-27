use super::*;
use crate::{
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
        .create(
            parent,
            crate::db::session_store::CreateOptions {
                model: Some("fixture".into()),
                provider: Some("fixture".into()),
                ..Default::default()
            },
        )
        .unwrap();
    let private_root = context.paths.data_dir.join("data/workspaces");
    let cwd = context
        .db
        .engine_workspace(&private_root, parent, None)
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
    .with_search_config_path(context.paths.layout().config_file("settings.json"))
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
    let memory_dir = context.paths.layout().agent_memory().join("analyst");
    std::fs::create_dir_all(&memory_dir).unwrap();
    std::fs::write(memory_dir.join("notes.md"), "MEMORY_SENTINEL").unwrap();
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

#[tokio::test]
async fn sdk_agent_invocation_rebinds_and_its_retained_executor_rejects_rotation() {
    use crate::native_engine::{provider_manager::ProviderManager, runtime_agents::AgentModel};
    async fn mock() -> (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
        let keys = Arc::new(Mutex::new(Vec::new()));
        let captured = keys.clone();
        let app = Router::new().route("/v1/chat/completions", post(move |headers: axum::http::HeaderMap| {
            captured.lock().unwrap().push(headers["authorization"].to_str().unwrap().to_owned());
            async { ([("content-type", "text/event-stream")],
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n") }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        (
            url,
            keys,
            tokio::spawn(async move { axum::serve(listener, app).await.unwrap() }),
        )
    }
    let (old, first_requests, first_server) = mock().await;
    let (new, next_requests, next_server) = mock().await;
    let dir = tempfile::tempdir().unwrap();
    let parent = uuid::Uuid::new_v4().to_string();
    let (mut host, _) = executor(dir.path(), &parent, old.clone(), Default::default()).await;
    let mut owner = host.provider.provider.clone();
    owner.models.push(host.provider.model.clone());
    let mut child = owner.clone();
    child.id = "explicit-child".into();
    child.api_key = "old-dummy".into();
    let manager = Arc::new(tokio::sync::Mutex::new(ProviderManager::in_memory(
        vec![owner, child.clone()],
        crate::data_layout::DataLayout::from_root(&dir.path()).config_file("providers.json"),
    )));
    host.provider_manager = Some(manager.clone());
    Arc::make_mut(&mut host.runtime_agents)[0].provider = Some(AgentModel {
        provider_id: child.id.clone(),
        model: "fixture".into(),
    });
    let invoke = json!({"prompt":"inspect","agent_type":"analyst"});
    let first = host.execute_delegated(&invoke).await.unwrap();
    child.base_url = new;
    child.api_key = "new-dummy".into();
    manager
        .lock()
        .await
        .update_provider("explicit-child", child);
    let err = host
        .resume_owned_child(
            first["session_id"].as_str().unwrap(),
            Some("continue"),
            false,
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("needs_resume"));
    host.execute_delegated(&invoke).await.unwrap();
    assert_eq!(*first_requests.lock().unwrap(), ["Bearer old-dummy"]);
    assert_eq!(*next_requests.lock().unwrap(), ["Bearer new-dummy"]);
    first_server.abort();
    next_server.abort();
}
