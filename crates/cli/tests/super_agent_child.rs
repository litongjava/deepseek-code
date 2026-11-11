//! A real `dcl` child must run in the prepared workspace, persist it, and
//! resume there. The model is a local HTTP mock that makes the child report its
//! own working directory through the Bash tool.
use deepseek_code_core::{
    native_engine::{
        child_session::{
            owner_record, spawn_child_session, ChildLaunchConfig, ChildSpawn, ChildWorkspace,
        },
        provider_manager::ResolvedProvider,
    },
    runtime::{RuntimePaths, SessionStore},
};
use serde_json::{json, Value};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

fn stream(content: Value, finish: &str) -> String {
    format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"index":0,"delta":content,"finish_reason":null}]}),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":finish}]})
    )
}

fn call(id: &str, name: &str, input: Value, index: usize) -> Value {
    json!({"index":index,"id":id,"type":"function","function":{"name":name,"arguments":input.to_string()}})
}

async fn wait_for_job(data: &Path, job_id: &str) -> String {
    let state_path = data
        .join("background-tasks")
        .join(job_id)
        .join("state.json");
    for _ in 0..480 {
        if let Ok(jobs) = deepseek_code_core::services::background_sessions::list(data, None) {
            if let Some(job) = jobs.iter().find(|job| job.id == job_id) {
                if matches!(
                    job.status.as_str(),
                    "completed" | "failed" | "cancelled" | "unresponsive"
                ) {
                    if job.status != "completed" {
                        panic!(
                            "child job ended as {}: {}",
                            job.status,
                            std::fs::read_to_string(&state_path).unwrap_or_default()
                        );
                    }
                    return job.status.clone();
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!(
        "child job did not finish: {}",
        std::fs::read_to_string(&state_path).unwrap_or_default()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_child_runs_in_the_prepared_workspace_and_resumes_there() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let parent_workspace = temp.path().join("project");
    let child_workspace = temp.path().join("task-workspace");
    for path in [&data, &parent_workspace, &child_workspace] {
        std::fs::create_dir_all(path).unwrap();
    }
    std::fs::write(child_workspace.join("marker.txt"), "prepared").unwrap();
    let first_out = temp.path().join("pwd-first.txt");
    let second_out = temp.path().join("pwd-second.txt");

    let mock_first = first_out.clone();
    let mock_second = second_out.clone();
    let seen = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = seen.clone();
    let app = axum::Router::new().route(
        "/v1/chat/completions",
        axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
            let first_out = mock_first.clone();
            let second_out = mock_second.clone();
            let captured = captured.clone();
            async move {
                captured.lock().unwrap().push(body.clone());
                let messages = body["messages"].as_array().cloned().unwrap_or_default();
                // History resumes with earlier tool results; only a tool result
                // after the latest user message belongs to this turn.
                let last_user = messages
                    .iter()
                    .rposition(|message| message["role"] == "user")
                    .unwrap_or(0);
                let has_tool_result = messages[last_user.saturating_add(1)..]
                    .iter()
                    .any(|message| message["role"] == "tool");
                let prompt = messages
                    .iter()
                    .rev()
                    .find(|message| message["role"] == "user")
                    .and_then(|message| message["content"].as_str())
                    .unwrap_or_default()
                    .to_owned();
                let response = if has_tool_result {
                    stream(json!({"content":"reported"}), "stop")
                } else {
                    let out = if prompt.contains("second") {
                        &second_out
                    } else {
                        &first_out
                    };
                    let command = format!("pwd > {}", out.display());
                    stream(
                        json!({"tool_calls":[call(
                            "pwd-1",
                            "Bash",
                            json!({"shell":"sh","command":command,"timeout":10000}),
                            0
                        )]}),
                        "tool_calls",
                    )
                };
                ([("content-type", "text/event-stream")], response)
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    // The child CLI resolves its provider from the shared configuration.
    std::fs::write(
        data.join("config.json"),
        serde_json::to_vec(&json!({
            "providers":[{
                "id":"fixture","name":"Fixture","provider_type":"openai","api_key":null,
                "base_url":format!("http://{address}"),
                "models":[{
                    "id":"fixture","name":"Fixture","enabled":true,"max_tokens":4096,
                    "supports_tools":true,"supports_streaming":true,
                    "context_window":128000,"cost_per_1k_input":null,"cost_per_1k_output":null
                }],
                "enabled":true,"is_default":true,"settings":{},
                "supports_web_search":false,"web_search_strategy":null,
                "web_search_tested_at":null,"web_search_test_reason":null
            }],
            "default_provider_id":"fixture"
        }))
        .unwrap(),
    )
    .unwrap();

    let paths = RuntimePaths::new(data.clone()).unwrap();
    let db = paths.open_database().unwrap();
    let sessions = SessionStore::new(db.clone());
    let parent = sessions
        .create(&parent_workspace, "fixture", "fixture")
        .unwrap();
    let provider = ResolvedProvider {
        provider: serde_json::from_value(json!({
            "id":"fixture","name":"Fixture","base_url":format!("http://{address}"),
            "api_key":"fixture-key","api_format":"openai","models":[],"enabled":true
        }))
        .unwrap(),
        model: serde_json::from_value(json!({
            "id":"fixture","name":"Fixture","enabled":true,
            "supports_vision":false,"supports_web_search":false
        }))
        .unwrap(),
    };
    let executable = std::path::PathBuf::from(env!("CARGO_BIN_EXE_dcl"));
    let request = |prompt: &str, resume: Option<&str>| ChildSpawn {
        data_dir: data.clone(),
        parent_session_id: parent.clone(),
        parent_workspace: parent_workspace.clone(),
        workspace: ChildWorkspace::Directory(child_workspace.clone()),
        prompt: prompt.into(),
        description: "cwd probe".into(),
        agent_type: "general".into(),
        model: None,
        effort: None,
        system_prompt_append: None,
        title: "cwd probe".into(),
        resume_session: resume.map(str::to_owned),
        extra_env: Vec::new(),
        executable_override: Some(executable.clone()),
        invocation: None,
        member_binding: None,
    };

    let first = spawn_child_session(
        ChildLaunchConfig {
            provider: provider.clone(),
            permission_mode: "bypass".into(),
        },
        request("first run", None),
    )
    .await
    .unwrap();
    wait_for_job(&data, &first.job_id).await;
    let canonical = child_workspace.canonicalize().unwrap();
    assert_eq!(
        std::fs::read_to_string(&first_out).unwrap().trim(),
        canonical.to_string_lossy(),
        "the first child turn must run in the prepared workspace"
    );
    assert!(
        std::fs::read_dir(&parent_workspace)
            .unwrap()
            .next()
            .is_none(),
        "the child must not write into the parent workspace"
    );
    let stored = sessions
        .db
        .with_conn(|conn| {
            deepseek_code_core::db::conversation_repo::get_conversation(conn, &first.session_id)
        })
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        stored.workspace_path.as_deref(),
        Some(canonical.to_string_lossy().as_ref())
    );
    assert_eq!(
        owner_record(
            &deepseek_code_core::services::ServiceContext::open(
                paths.clone(),
                Arc::new(deepseek_code_core::services::EnvironmentCredentials)
            )
            .unwrap(),
            &first.session_id
        )
        .unwrap()
        .unwrap()
        .parent_session_id,
        parent
    );

    let second = spawn_child_session(
        ChildLaunchConfig {
            provider,
            permission_mode: "bypass".into(),
        },
        request("second run", Some(&first.session_id)),
    )
    .await
    .unwrap();
    assert_eq!(second.session_id, first.session_id);
    wait_for_job(&data, &second.job_id).await;
    if let Err(error) = std::fs::read_to_string(&second_out) {
        let state = std::fs::read_to_string(
            data.join("background-tasks")
                .join(&second.job_id)
                .join("state.json"),
        )
        .unwrap_or_default();
        let summary: Vec<String> = seen
            .lock()
            .unwrap()
            .iter()
            .map(|request| {
                let messages = request["messages"].as_array().cloned().unwrap_or_default();
                let has_tool = messages.iter().any(|message| message["role"] == "tool");
                let last_user = messages
                    .iter()
                    .rev()
                    .find(|message| message["role"] == "user")
                    .and_then(|message| message["content"].as_str())
                    .unwrap_or_default()
                    .chars()
                    .take(120)
                    .collect::<String>();
                format!("tool_result={has_tool} last_user={last_user:?}")
            })
            .collect();
        panic!(
            "second child produced no pwd ({error}); state={state}; requests={summary:#?}; data={}",
            data.display()
        );
    }
    assert_eq!(
        std::fs::read_to_string(&second_out).unwrap().trim(),
        canonical.to_string_lossy(),
        "a resumed child must keep its recorded workspace"
    );
    // The completed first process was not left running, and resume reused the
    // same conversation instead of creating a second one.
    let jobs =
        deepseek_code_core::services::background_sessions::list(&data, Some(&parent)).unwrap();
    let live = jobs
        .iter()
        .filter(|job| job.session_id == first.session_id)
        .filter(|job| matches!(job.status.as_str(), "starting" | "running" | "unresponsive"))
        .count();
    assert_eq!(live, 0);
    let conversations: i64 = sessions
        .db
        .with_conn(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM conversations WHERE id <> ?1",
                [&parent],
                |row| row.get(0),
            )
        })
        .unwrap()
        .unwrap();
    assert_eq!(conversations, 1);
    assert!(seen.lock().unwrap().len() >= 4);
    server.abort();
}
