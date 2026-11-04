use super::*;

fn provider(base: &str, anthropic: bool) -> ResolvedProvider {
    crate::config::OrchestrationConfig::init_global().unwrap();
    ResolvedProvider {
        provider: serde_json::from_value(json!({"id":"resume-test","name":"resume-test","base_url":base,
            "api_key":"fixture-only","api_format":if anthropic {"anthropic"} else {"openai"},"enabled":true,"models":[]})).unwrap(),
        model: serde_json::from_value(json!({"id":"fixture","name":"fixture","context_window":8000,"enabled":true,"supports_web_search":false})).unwrap(),
    }
}

#[tokio::test]
async fn first_request_after_resume_is_compressed_for_both_protocols() {
    for anthropic in [false, true] {
        let seen = Arc::new(std::sync::Mutex::new(Value::Null));
        let captured = seen.clone();
        let app = axum::Router::new().route(
            "/{*path}",
            axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
                *captured.lock().unwrap() = body;
                async move {
                    let event = if anthropic {
                        json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}})
                    } else {
                        json!({"choices":[{"delta":{},"finish_reason":"stop"}]})
                    };
                    (
                        [("content-type", "text/event-stream")],
                        format!("data: {event}\n\n"),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut history = Vec::new();
        if !anthropic {
            history.push(json!({"role":"system","content":"workspace rules"}));
            history.push(json!({"role":"system","content":"permission rules"}));
        }
        for _ in 0..8 {
            history.push(json!({"role":"user","content":"old request"}));
            history.push(json!({"role":"assistant","content":"old answer ".repeat(12000)}));
        }
        history.push(json!({"role":"user","content":"Continue the current task"}));
        let (tx, mut rx) = mpsc::channel(128);
        let drain = tokio::spawn(async move { while rx.recv().await.is_some() {} });
        let system = "Base instructions must remain complete. ".repeat(100);
        let mut engine = ToolLoopExecutor::new(
            provider(&base, anthropic),
            history,
            Some(system.clone()),
            128,
            tx,
            ".".into(),
            ReasoningMode::Standard,
        );
        let result = if anthropic {
            engine.execute_anthropic_loop().await
        } else {
            engine.execute_openai_loop().await
        };
        result.unwrap();
        let body = seen.lock().unwrap().clone();
        assert!(
            body["messages"].to_string().len() < 10000,
            "First resumed request must not send the whole old transcript"
        );
        assert!(body["messages"]
            .to_string()
            .contains("Continue the current task"));
        if anthropic {
            assert!(body["system"].to_string().contains(&system));
            assert!(body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .all(|message| message["role"] != "system"));
        } else {
            let instructions = body["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|message| message["role"] == "system")
                .map(Value::to_string)
                .collect::<String>();
            for required in [&system, "workspace rules", "permission rules"] {
                assert!(
                    instructions.contains(required),
                    "Missing instruction: {required}"
                );
            }
        }
        drop(engine);
        drain.await.unwrap();
        server.abort();
    }
}

#[test]
fn empty_argument_delta_preserves_complete_input_but_not_a_placeholder() {
    let (tx, _rx) = mpsc::channel(128);
    let mut engine = ToolLoopExecutor::new(
        provider("http://127.0.0.1:1", true),
        vec![],
        None,
        128,
        tx,
        ".".into(),
        ReasoningMode::Standard,
    );
    for input in [json!({}), json!({"file_path":"a.txt"})] {
        engine.handle_streaming_tool_arg_delta("call", "Read", &input.to_string());
        engine.handle_streaming_tool_arg_delta("call", "Read", "");
        assert_eq!(
            engine.finalize_streaming_tool_args("call"),
            if input == json!({}) {
                Value::Null
            } else {
                input
            }
        );
    }
    assert_eq!(engine.finalize_streaming_tool_args("missing"), Value::Null);
}

#[test]
fn context_estimate_counts_structured_tool_payloads() {
    for message in [
        json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"call","content":"x".repeat(30000)}]}),
        json!({"role":"assistant","content":null,"tool_calls":[{"id":"call","function":{"name":"Write","arguments":"x".repeat(30000)}}]}),
    ] {
        assert!(TokenCounter::estimate_message(&message) >= 10000);
    }
    let image =
        json!({"role":"user","content":[{"type":"image","source":{"data":"x".repeat(30000)}}]});
    assert!(TokenCounter::estimate_message(&image) < 100);
}

#[test]
fn request_trace_preserves_initial_arguments_and_explains_eof() {
    let mut trace = RequestTrace::new(json!({"id":"trace","startedAt":0}));
    trace.observe_anthropic(&json!({"type":"content_block_start","index":0,
        "content_block":{"type":"tool_use","id":"call","name":"Read","input":{"file_path":"a.txt"}}}));
    trace.observe_anthropic(&json!({"type":"content_block_delta","index":0,
        "delta":{"type":"input_json_delta","partial_json":""}}));
    let snapshot = trace.finish("completed", None);
    assert_eq!(snapshot["responseBlocks"][0]["input"]["file_path"], "a.txt");
    assert_eq!(snapshot["status"], "interrupted");
    assert!(snapshot["error"]
        .as_str()
        .unwrap()
        .contains("without a completion marker"));
}

#[tokio::test]
async fn non_sse_success_responses_report_gateway_errors_for_both_clients() {
    for content_type in ["application/json", "text/html"] {
        let app = axum::Router::new().route(
            "/{*path}",
            axum::routing::post(move || async move {
                (
                    [("content-type", content_type)],
                    if content_type == "application/json" {
                        r#"{"error":{"message":"fixture gateway rejected tool history"}}"#
                    } else {
                        "<html>upstream unavailable</html>"
                    },
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let selected = provider(&base, true);
        let streams = [
            AnthropicClient::new()
                .send_message_stream(&selected, vec![], None, vec![], 128)
                .await
                .unwrap(),
            AnthropicClient::new()
                .send_message_stream_with_thinking(&selected, vec![], None, vec![], 128, false)
                .await
                .unwrap(),
            OpenAIClient::new()
                .send_message_stream(&selected, vec![], None, vec![], 128)
                .await
                .unwrap(),
        ];
        for mut stream in streams {
            let error = stream.next().await.unwrap().unwrap_err().to_string();
            assert!(error.contains("HTTP 200"), "{error}");
            assert!(error.contains(content_type), "{error}");
            if content_type == "application/json" {
                assert!(error.contains("fixture gateway rejected tool history"));
            }
            assert!(stream.next().await.is_none());
        }
        server.abort();
    }
}
