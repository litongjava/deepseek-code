use super::*;

#[tokio::test]
async fn fresh_read_retains_requested_source_range_beyond_spill_limit() {
    let dir = tempfile::tempdir().unwrap();
    let source = (1..=400)
        .map(|line| format!("const line_{line} = '完整 source content beyond preview';\n"))
        .collect::<String>();
    std::fs::write(dir.path().join("source.ts"), &source).unwrap();
    let (tx, _rx) = mpsc::channel(16);
    let mut executor = ToolLoopExecutor::new(
        provider("http://127.0.0.1:1", "openai"),
        vec![],
        None,
        128,
        tx,
        dir.path().to_string_lossy().into(),
        ReasoningMode::Standard,
    )
    .with_search_config_path(dir.path().join("settings.json"));
    let (_, output, failed) = executor
        .execute_tool_call("Read", &json!({"file_path":"source.ts"}), "read-source")
        .await;
    assert!(!failed, "{output}");
    assert!(output.len() > 16_000);
    let retained = executor.retained_tool_output("Read", &output);
    assert_eq!(retained, output);
    assert!(retained.contains("line_400"));
    assert!(!dir.path().join("tool-outputs").exists());

    let (_, range, failed) = executor
        .execute_tool_call(
            "Read",
            &json!({"file_path":"source.ts", "offset":350, "limit":51}),
            "read-range",
        )
        .await;
    assert!(!failed, "{range}");
    assert_eq!(executor.retained_tool_output("Read", &range), range);
    assert!(range.contains("line_400"));
    assert!(!range.contains("line_349"));

    let write = executor.retained_tool_output(
        "Write",
        &json!({
            "success":true, "file_change":{"before":source,"after":"replacement"},
        })
        .to_string(),
    );
    assert_eq!(
        serde_json::from_str::<Value>(&write).unwrap(),
        json!({"success":true})
    );
}

#[tokio::test]
async fn immediate_terminal_answers_are_registered_before_events() {
    let dir = tempfile::tempdir().unwrap();
    for permission in [false, true] {
        let (tx, mut rx) = mpsc::channel(16);
        let mut executor = ToolLoopExecutor::new(provider("http://127.0.0.1:1", "openai"), vec![], None, 128, tx, dir.path().to_string_lossy().into(), ReasoningMode::Standard);
        let waiters = executor.get_answer_waiters();
        let task = tokio::spawn(async move {
            if permission {
                executor.execute_ask_user_confirmation("Write", &json!({"file_path":"approved.txt","content":"approved"}), "call", "approve").await
            } else {
                executor.execute_ask_user_question(&json!({"question":"Which option?","options":[]}), "question-fixture").await
            }
        });
        let event = rx.recv().await.unwrap();
        let request_id = match event {
            EngineEvent::AskUser { request_id, .. } | EngineEvent::ToolPermission { request_id, .. } => request_id,
            other => panic!("Unexpected event: {other:?}"),
        };
        let sender = waiters.lock().await.remove(&request_id).expect("Answer channel must exist when event arrives");
        sender.send(if permission { "yes" } else { "selected" }.into()).unwrap();
        let (_, output, is_error) = tokio::time::timeout(Duration::from_secs(5), task).await.unwrap().unwrap();
        assert!(!is_error, "{output}");
        if permission { assert_eq!(std::fs::read_to_string(dir.path().join("approved.txt")).unwrap(), "approved"); }
        else { assert!(output.contains("selected")); }
    }
}

fn provider(base: &str, format: &str) -> ResolvedProvider {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| crate::config::OrchestrationConfig::init_global().unwrap());
    ResolvedProvider {
        provider: serde_json::from_value(json!({
            "id": "code-test", "name": "code-test", "base_url": base,
            "api_key": "local-test-key", "api_format": format, "models": [], "enabled": true
        })).unwrap(),
        model: serde_json::from_value(json!({"id": "code-test", "name": "code-test", "enabled": true,
            "supports_vision": false, "supports_web_search": false})).unwrap(),
    }
}

#[tokio::test]
async fn all_modes_preserve_memory_and_settings_in_either_builder_order() {
    let temp = tempfile::tempdir().unwrap();
    let (tx, _rx) = mpsc::channel(16);
    for mode in ["chat", "work", "study", "code"] {
    for memory_first in [true, false] {
        let client = MemExClient::with_path(temp.path().join("memory.sqlite3"), None);
        let mut config = client.get_config().await;
        config.auto_search = false;
        config.auto_ingest = false;
        config.search_top_k = 7;
        client.update_config(config).await.unwrap();
        let profile = PromptProfile::from_workspace_mode(mode);
        let executor = ToolLoopExecutor::new(provider("http://127.0.0.1:1", "openai"), vec![], None, 128, tx.clone(), temp.path().to_string_lossy().into(), ReasoningMode::Standard);
        let mut executor = if memory_first {
            executor.with_memex(client.clone()).with_prompt_profile(profile)
        } else {
            executor.with_prompt_profile(profile).with_memex(client.clone())
        };
        assert!(executor.memex_client.is_some());
        assert!(executor.context_manager.is_some());
        let config = executor.memex_client.as_ref().unwrap().get_config().await;
        assert!(!config.auto_search && !config.auto_ingest);
        assert_eq!(config.search_top_k, 7);
        for name in ["memory_ingest", "memory_search"] {
            let (_, output, is_error) = executor.execute_tool_call(name, &json!({"content":"remember native memory", "query":"native memory", "importance":0.9}), "test").await;
            assert!(!is_error, "{mode}: {output}");
            assert!(output.contains(if name == "memory_ingest" { "ingested" } else { "remember native memory" }));
        }
    }
    }
}

/// Real HTTP/SSE requests against a local mock, with no external model credentials.
/// Exercise real memory tools, automatic recall and ingestion in all four modes.
#[tokio::test]
async fn all_modes_recall_ingest_and_run_memory_tools_on_both_protocols() {
    use axum::{http::header, routing::post, Json, Router};
    for format in ["anthropic", "openai"] {
    for mode in ["chat", "work", "study", "code"] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
        let requests = captured.clone();
        let app = Router::new().route(if format == "anthropic" { "/v1/messages" } else { "/v1/chat/completions" }, post(move |Json(body): Json<Value>| {
            let requests = requests.clone();
            async move {
                let mut requests = requests.lock().await;
                let step = requests.len();
                let first = step < 2;
                let tool_name = if step == 0 { "memory_ingest" } else { "memory_search" };
                let arguments = if step == 0 { json!({"content":"remember toolmemory", "importance":0.9}) } else { json!({"query":"toolmemory"}) }.to_string();
                requests.push(body);
                let events = if format == "anthropic" {
                    if first {
                        vec![json!({"type":"message_start","message":{"id":"test","model":"code-test","usage":{"input_tokens":20}}}),
                            json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":format!("memory-{step}"),"name":tool_name,"input":{}}}),
                            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":arguments}}),
                            json!({"type":"content_block_stop","index":0}),
                            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":10}}),
                            json!({"type":"message_stop"})]
                    } else {
                        vec![json!({"type":"message_start","message":{"id":"test2","model":"code-test","usage":{"input_tokens":20}}}),
                            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"verified"}}),
                            json!({"type":"content_block_stop","index":0}),
                            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":10}}),
                            json!({"type":"message_stop"})]
                    }
                } else if first {
                    vec![json!({"id":"test","model":"code-test","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":format!("memory-{step}"),"type":"function","function":{"name":tool_name,"arguments":arguments}}]},"finish_reason":null}]}),
                        json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]})]
                } else {
                    vec![json!({"id":"test2","model":"code-test","choices":[{"index":0,"delta":{"content":"verified"},"finish_reason":null}]}),
                        json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})]
                };
                let mut stream = events.into_iter().map(|event| format!("data: {event}\n\n")).collect::<String>();
                if format == "openai" { stream.push_str("data: [DONE]\n\n"); }
                ([(header::CONTENT_TYPE, "text/event-stream")], stream)
            }
        }));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
        let (tx, mut rx) = mpsc::channel(128);
        let temp = tempfile::tempdir().unwrap();
        let client = MemExClient::with_path(temp.path().join("memory.sqlite3"), None);
        let config = client.get_config().await;
        assert!(config.enabled && config.auto_search && config.auto_ingest);
        client.ingest("recallmarker existing architecture", Some(0.9), None).await.unwrap();
        let profile = PromptProfile::from_workspace_mode(mode);
        let prompt = crate::prompt::compose_system_prompt(Some("custom project rule"), "code-test", profile);
        let mut executor = ToolLoopExecutor::new(provider(&base, format), vec![json!({"role":"user","content":"recallmarker"})], Some(prompt), 128, tx, ".".into(), ReasoningMode::Standard)
            .with_prompt_profile(profile).with_memex(client.clone()).with_conv_id(format!("{mode}-{format}"));
        let (text, _) = tokio::time::timeout(Duration::from_secs(15), executor.execute()).await.unwrap().unwrap();
        server.abort();
        assert_eq!(text, "verified", "{format}");
        let requests = captured.lock().await;
        assert_eq!(requests.len(), 3, "{format} {mode}");
        let first = &requests[0];
        let system = if format == "anthropic" { first["system"].to_string() } else { first["messages"][0]["content"].to_string() };
        assert_eq!(system.contains("<code_behavior>"), mode == "code");
        assert!(system.contains("custom project rule") && system.contains("<memory_guidance>"));
        assert!(first["tools"].to_string().contains("memory_ingest"));
        assert!(first["tools"].to_string().contains("memory_search"));
        assert!(first["messages"].to_string().contains("recallmarker existing architecture"));
        assert!(requests[1]["messages"].to_string().contains("ingested"));
        assert!(requests[2]["messages"].to_string().contains("search_completed"));
        assert!(requests[2]["messages"].to_string().contains("remember toolmemory"));
        let saved = client.search("verified", None).await.unwrap();
        assert!(saved.iter().any(|item| item.content.contains("Assistant: verified")), "automatic ingestion: {mode} {format}");
        let mut completed = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let EngineEvent::ToolUseDone { tool_name, is_error, .. } = event {
                if is_memory_tool(&tool_name) { assert!(!is_error); completed.push(tool_name); }
            }
        }
        assert_eq!(completed, vec!["memory_ingest", "memory_search"], "{mode} {format}");
    }
    }
}

/// Instructions and the Git snapshot open the first user message on both
/// protocols, stay there for later requests in the turn, and never reach
/// the executor's own message list.
#[tokio::test]
async fn context_reminders_open_the_first_user_message() {
    use axum::{http::header, routing::post, Json, Router};
    for format in ["anthropic", "openai"] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
        let requests = captured.clone();
        let app = Router::new().route(if format == "anthropic" { "/v1/messages" } else { "/v1/chat/completions" }, post(move |Json(body): Json<Value>| {
            let requests = requests.clone();
            async move {
                let mut requests = requests.lock().await;
                let first = requests.is_empty();
                requests.push(body);
                let events = match (format, first) {
                    ("anthropic", true) => vec![json!({"type":"message_start","message":{"id":"a","model":"code-test","usage":{"input_tokens":1}}}),
                        json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"glob-1","name":"Glob","input":{}}}),
                        json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"pattern\":\"*.none\"}"}}),
                        json!({"type":"content_block_stop","index":0}),
                        json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":1}}),
                        json!({"type":"message_stop"})],
                    ("anthropic", false) => vec![json!({"type":"message_start","message":{"id":"b","model":"code-test","usage":{"input_tokens":1}}}),
                        json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
                        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"done"}}),
                        json!({"type":"content_block_stop","index":0}),
                        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":1}}),
                        json!({"type":"message_stop"})],
                    (_, true) => vec![json!({"id":"a","model":"code-test","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"glob-1","type":"function","function":{"name":"Glob","arguments":"{\"pattern\":\"*.none\"}"}}]},"finish_reason":null}]}),
                        json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]})],
                    _ => vec![json!({"id":"b","model":"code-test","choices":[{"index":0,"delta":{"content":"done"},"finish_reason":null}]}),
                        json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]})],
                };
                let mut stream = events.into_iter().map(|event| format!("data: {event}\n\n")).collect::<String>();
                if format == "openai" { stream.push_str("data: [DONE]\n\n"); }
                ([(header::CONTENT_TYPE, "text/event-stream")], stream)
            }
        }));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap(); });
        let (tx, _rx) = mpsc::channel(128);
        let temp = tempfile::tempdir().unwrap();
        let reminders = vec!["<system-reminder>\nreminder-marker-rules\n</system-reminder>".to_string(), "<system-reminder>\n# gitStatus\n</system-reminder>".to_string()];
        let prompt = crate::prompt::compose_system_prompt(None, "code-test", PromptProfile::Code);
        let history = vec![json!({"role":"user","content":"hello"}), json!({"role":"assistant","content":"hi"}), json!({"role":"user","content":"again"})];
        // Recorded on an earlier turn for "hello", and on this turn for "again".
        let turn_context = vec![
            crate::services::turn_context::for_last(&history[..1], vec!["hook-for-hello".into()]).unwrap(),
            crate::services::turn_context::for_last(&history, vec!["skill-for-again".into()]).unwrap(),
        ];
        let mut executor = ToolLoopExecutor::new(provider(&base, format), history.clone(), Some(prompt), 128, tx, temp.path().to_string_lossy().into(), ReasoningMode::Standard)
            .with_prompt_profile(PromptProfile::Code).with_context_reminders(reminders.clone()).with_turn_context(turn_context).with_conv_id(format!("reminders-{format}"));
        let (text, _) = tokio::time::timeout(Duration::from_secs(15), executor.execute()).await.unwrap().unwrap();
        server.abort();
        assert_eq!(text, "done", "{format}");
        let requests = captured.lock().await;
        assert_eq!(requests.len(), 2, "{format}");
        for request in requests.iter() {
            if format == "anthropic" {
                let content = &request["messages"][0]["content"];
                assert_eq!(content[0]["text"], reminders[0].as_str());
                assert_eq!(content[1]["text"], reminders[1].as_str());
                assert_eq!(content[2]["text"], "hello");
                assert_eq!(content[3]["text"], "hook-for-hello");
                let texts: Vec<_> = request["messages"][2]["content"].as_array().unwrap().iter().map(|block| block["text"].clone()).collect();
                assert_eq!(texts, [json!("again"), json!("skill-for-again")]);
                assert!(!request["system"].to_string().contains("reminder-marker"));
            } else {
                let users: Vec<_> = request["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "user").collect();
                assert_eq!(users[0]["content"], format!("{}\n\n{}\n\nhello\n\nhook-for-hello", reminders[0], reminders[1]).as_str());
                assert_eq!(users[1]["content"], "again\n\nskill-for-again");
            }
        }
        assert_eq!(executor.messages[0], history[0], "{format}: the transcript keeps the user's own text");
    }
}
