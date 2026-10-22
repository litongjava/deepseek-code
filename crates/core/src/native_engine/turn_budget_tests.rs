use super::*;

async fn check_budget(format: ApiFormat, budget: u32) {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let captured = count.clone();
    let anthropic = matches!(format, ApiFormat::Anthropic);
    let app=axum::Router::new().route(if anthropic {"/v1/messages"} else {"/v1/chat/completions"},axum::routing::post(move || {
        let captured=captured.clone();
        async move {
            let first=captured.fetch_add(1,Ordering::SeqCst)==0;
            let body=if anthropic {
                let events=if first {
                    vec![json!({"type":"message_start","message":{"id":"fixture","type":"message","role":"assistant","content":[],"model":"fixture","usage":{"input_tokens":10,"output_tokens":0}}}),json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"read-1","name":"Read","input":{}}}),json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"file_path\":\"fixture.txt\"}"}}),json!({"type":"content_block_stop","index":0}),json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":10}}),json!({"type":"message_stop"})]
                } else {
                    vec![json!({"type":"message_start","message":{"id":"fixture-2","type":"message","role":"assistant","content":[],"model":"fixture","usage":{"input_tokens":20,"output_tokens":0}}}),json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"done"}}),json!({"type":"content_block_stop","index":0}),json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),json!({"type":"message_stop"})]
                };
                events.into_iter().map(|event|format!("event: {}\ndata: {event}\n\n",event["type"].as_str().unwrap())).collect::<String>()
            } else if first {
                format!("data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",json!({"id":"fixture","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"read-1","type":"function","function":{"name":"Read","arguments":"{\"file_path\":\"fixture.txt\"}"}}]},"finish_reason":null}]}),json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}))
            } else {
                format!("data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",json!({"choices":[{"index":0,"delta":{"content":"done"},"finish_reason":null}]}),json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}))
            };
            ([("content-type","text/event-stream")],body)
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("fixture.txt"), "actual tool fixture").unwrap();
    let provider=ResolvedProvider {
        provider:serde_json::from_value(json!({"id":"budget-test","name":"budget-test","base_url":format!("http://{addr}"),"api_key":"unused","api_format":if anthropic {"anthropic"} else {"openai"},"models":[],"enabled":true})).unwrap(),
        model:serde_json::from_value(json!({"id":"fixture","name":"fixture","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap(),
    };
    let (tx, mut rx) = mpsc::channel(128);
    let mut executor = ToolLoopExecutor::new(
        provider,
        vec![json!({"role":"user","content":"Read fixture.txt then answer"})],
        None,
        128,
        tx,
        dir.path().to_string_lossy().into_owned(),
        ReasoningMode::Standard,
    )
    .with_max_turns(Some(budget))
    .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        if anthropic {
            executor.execute_anthropic_loop().await
        } else {
            executor.execute_openai_loop().await
        }
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(count.load(Ordering::SeqCst), budget as usize);
    let mut limited = false;
    let mut read_executed = false;
    let mut complete_starts = 0;
    while let Ok(event) = rx.try_recv() {
        match event {
            EngineEvent::ToolUseStart { tool_name, tool_input, .. } if tool_name == "Read" => {
                assert!(!read_executed, "Complete input must precede tool completion");
                assert_eq!(tool_input["file_path"], "fixture.txt");
                complete_starts += 1;
            }
            EngineEvent::Error(message) if message.contains("Max tool iterations") => {
                limited = true
            }
            EngineEvent::ToolUseDone {
                output,
                is_error: false,
                ..
            } if output.contains("actual tool fixture") => read_executed = true,
            _ => {}
        }
    }
    assert_eq!(complete_starts, 1, "Exactly one start with complete arguments is emitted");
    assert!(
        read_executed,
        "Budget tests must execute the real Read tool"
    );
    if budget == 1 {
        assert_eq!(result.1.as_deref(), Some("max_iterations"));
        assert!(limited, "Budget exhaustion must be visible to the caller");
    } else {
        assert_eq!(result.0, "done");
    }
    server.abort();
}

#[tokio::test]
async fn openai_stops_at_requested_turn_budget() {
    check_budget(ApiFormat::OpenAI, 1).await;
    check_budget(ApiFormat::OpenAI, 2).await;
}

#[tokio::test]
async fn anthropic_stops_at_requested_turn_budget() {
    check_budget(ApiFormat::Anthropic, 1).await;
    check_budget(ApiFormat::Anthropic, 2).await;
}

async fn check_structured_output(anthropic: bool, valid: bool) {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let seen = Arc::new(std::sync::Mutex::new(Value::Null));
    let captured = seen.clone();
    let text = if valid {
        r#"{"answer":42}"#
    } else {
        r#"{"answer":"wrong"}"#
    };
    let app=axum::Router::new().route(if anthropic {"/v1/messages"} else {"/v1/chat/completions"},axum::routing::post(move |axum::Json(body):axum::Json<Value>| {
        let captured=captured.clone();
        async move {
            *captured.lock().unwrap()=body;
            let body=if anthropic {
                vec![json!({"type":"message_start","message":{"id":"fixture","type":"message","role":"assistant","content":[],"model":"fixture","usage":{"input_tokens":10,"output_tokens":0}}}),json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}),json!({"type":"content_block_stop","index":0}),json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),json!({"type":"message_stop"})].into_iter().map(|event|format!("event: {}\ndata: {event}\n\n",event["type"].as_str().unwrap())).collect::<String>()
            } else {
                format!("data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",json!({"choices":[{"index":0,"delta":{"content":text},"finish_reason":null}]}),json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}))
            };
            ([("content-type","text/event-stream")],body)
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let mut provider=ResolvedProvider {
        provider:serde_json::from_value(json!({"id":"schema-test","name":"schema-test","base_url":format!("http://{addr}"),"api_key":"unused","api_format":if anthropic {"anthropic"} else {"openai"},"models":[],"enabled":true})).unwrap(),
        model:serde_json::from_value(json!({"id":"claude-sonnet-4-6","name":"fixture","effort":"high","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap(),
    };
    provider.model.effort = Some("high".into());
    let schema = json!({"type":"object","properties":{"answer":{"type":"integer"}},"required":["answer"],"additionalProperties":false});
    let dir = tempfile::tempdir().unwrap();
    let (tx, mut rx) = mpsc::channel(128);
    let mut executor = ToolLoopExecutor::new(
        provider,
        vec![json!({"role":"user","content":"Return an answer"})],
        None,
        128,
        tx,
        dir.path().to_string_lossy().into_owned(),
        ReasoningMode::Standard,
    )
    .with_response_schema(Some(schema.clone()))
    .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(10), executor.execute())
        .await
        .unwrap();
    let request = seen.lock().unwrap().clone();
    if anthropic {
        assert_eq!(
            request["output_config"]["format"],
            json!({"type":"json_schema","schema":schema})
        );
        assert_eq!(request["output_config"]["effort"], "high");
    } else {
        assert_eq!(request["response_format"]["json_schema"]["schema"], schema);
        assert_eq!(request["response_format"]["json_schema"]["strict"], true);
    }
    if valid {
        assert_eq!(result.unwrap().0, text);
    } else {
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("does not match JSON schema"));
        let mut failed = false;
        while let Ok(event) = rx.try_recv() {
            if matches!(event, EngineEvent::Error(_)) {
                failed = true;
            }
        }
        assert!(
            failed,
            "Invalid structured output must emit an engine error"
        );
    }
    server.abort();
}

#[tokio::test]
async fn providers_receive_strict_schema_and_invalid_results_fail() {
    for anthropic in [false, true] {
        for valid in [false, true] {
            check_structured_output(anthropic, valid).await;
        }
    }
}

async fn check_output_limit_recovery(anthropic: bool) {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let requests = Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    let captured = requests.clone();
    let app=axum::Router::new().route(if anthropic {"/v1/messages"} else {"/v1/chat/completions"},axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
        let captured=captured.clone();
        async move {
            let first={let mut requests=captured.lock().unwrap();requests.push(body);requests.len()==1};
            let body=if anthropic {
                let events=if first {
                    vec![json!({"type":"message_start","message":{"id":"cut","type":"message","role":"assistant","content":[],"model":"fixture","usage":{"input_tokens":10,"output_tokens":0}}}),json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Part one. "}}),json!({"type":"content_block_stop","index":0}),json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"write-1","name":"Write","input":{}}}),json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"file_path\":\"big.txt\",\"content\":\"aaaa"}}),json!({"type":"content_block_stop","index":1}),json!({"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":128}}),json!({"type":"message_stop"})]
                } else {
                    vec![json!({"type":"message_start","message":{"id":"resume","type":"message","role":"assistant","content":[],"model":"fixture","usage":{"input_tokens":20,"output_tokens":0}}}),json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"done"}}),json!({"type":"content_block_stop","index":0}),json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),json!({"type":"message_stop"})]
                };
                events.into_iter().map(|event|format!("event: {}\ndata: {event}\n\n",event["type"].as_str().unwrap())).collect::<String>()
            } else if first {
                format!("data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",json!({"choices":[{"index":0,"delta":{"content":"Part one. ","tool_calls":[{"index":0,"id":"write-1","type":"function","function":{"name":"Write","arguments":"{\"file_path\":\"big.txt\",\"content\":\"aaaa"}}]},"finish_reason":null}]}),json!({"choices":[{"index":0,"delta":{},"finish_reason":"length"}]}))
            } else {
                format!("data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",json!({"choices":[{"index":0,"delta":{"content":"done"},"finish_reason":null}]}),json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}))
            };
            ([("content-type","text/event-stream")],body)
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let provider=ResolvedProvider {
        provider:serde_json::from_value(json!({"id":"limit-test","name":"limit-test","base_url":format!("http://{addr}"),"api_key":"unused","api_format":if anthropic {"anthropic"} else {"openai"},"models":[],"enabled":true})).unwrap(),
        model:serde_json::from_value(json!({"id":"fixture","name":"fixture","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap(),
    };
    let (tx, mut rx) = mpsc::channel(128);
    let mut executor = ToolLoopExecutor::new(
        provider,
        vec![json!({"role":"user","content":"Write big.txt"})],
        None,
        128,
        tx,
        dir.path().to_string_lossy().into_owned(),
        ReasoningMode::Standard,
    )
    .with_max_turns(Some(5))
    .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        if anthropic { executor.execute_anthropic_loop().await } else { executor.execute_openai_loop().await }
    })
    .await
    .unwrap()
    .unwrap();
    assert!(result.0.ends_with("done"), "{}", result.0);
    assert!(!dir.path().join("big.txt").exists(), "a cut-off Write must not run");
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let resume = requests[1]["messages"].as_array().unwrap().last().unwrap().to_string();
    assert!(resume.contains("Output token limit hit (128 tokens)"), "{resume}");
    let mut warned = false;
    while let Ok(event) = rx.try_recv() {
        if let EngineEvent::BudgetWarning { message, .. } = event {
            warned |= message.contains("asking the model to continue");
        }
    }
    assert!(warned, "the user must see why the model is continuing");
    server.abort();
}

#[tokio::test]
async fn cut_off_responses_continue_without_running_partial_calls() {
    check_output_limit_recovery(true).await;
    check_output_limit_recovery(false).await;
}
