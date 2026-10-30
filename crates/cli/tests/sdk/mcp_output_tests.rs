use super::*;

const PNG: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+jA1sAAAAASUVORK5CYII=";

fn replacement() -> Value {
    json!([{"type":"text","text":"SDK_REDACTED"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":PNG}}])
}

fn vision_fixture(root: &Path, server: &Server) {
    let mut config = deepseek_code_core::config::ConfigManager::new(root.join("data"));
    config.add_provider(serde_json::from_value(json!({"id":"vision-fixture","name":"Vision fixture","provider_type":if server.fixture.anthropic {"anthropic"}else{"openai"},"base_url":server.url,"enabled":true,"is_default":false,"settings":{},"supports_web_search":false,"models":[{"id":"fixture-model","name":"fixture-model","enabled":true,"supports_vision":true,"supports_tools":true,"supports_text":true,"supports_streaming":true}]})).unwrap()).unwrap();
}

fn command_post_hook(root: &Path) {
    let script = root.join("post-hook.cjs");
    std::fs::write(&script, r#"let input='';process.stdin.on('data',d=>input+=d);process.stdin.on('end',()=>{const i=JSON.parse(input);if(i.tool_response[0].text!=='STDIO_RESULT')process.exit(7);process.stdout.write(JSON.stringify({hookSpecificOutput:{hookEventName:'PostToolUse',updatedMCPToolOutput:[{type:'text',text:'COMMAND_REDACTED'}]}}));});"#).unwrap();
    let command = format!("node \"{}\"", script.display());
    std::fs::write(root.join("data/settings.json"),json!({"hooks":{"PostToolUse":[{"matcher":"^mcp__native__stdio_echo$","hooks":[{"type":"command","command":command}]}]}}).to_string()).unwrap();
}

#[tokio::test]
async fn post_mcp_output_reaches_both_providers_display_storage_and_resume() {
    for anthropic in [false, true] {
        let root = tempfile::tempdir().unwrap();
        stdio_server(root.path());
        command_post_hook(root.path());
        let server = start_server(
            anthropic,
            vec![
                ("mcp__first__echo", json!({"value":"RAW_PRIVATE"})),
                ("mcp__native__stdio_echo", json!({"value":"CALLED_ONCE"})),
                ("Read", json!({"file_path":"fixture.txt"})),
                (
                    "ReadMcpResource",
                    json!({"server_id":"sdk:first","uri":"fixture:first"}),
                ),
            ],
        )
        .await;
        vision_fixture(root.path(), &server);
        let mut client = Client::start(root.path(), &server, &[]);
        let init = initialize(
            &mut client,
            json!(["first"]),
            json!({"PostToolUse":[{"hookCallbackIds":["post"]}]}),
        )
        .await;
        assert_eq!(init["response"]["subtype"], "success");
        client
            .send(user("Call SDK, stdio, native Read and resource tools"))
            .await;
        let mut frames = Vec::new();
        let result = loop {
            let frame = next(&mut client).await;
            if frame["type"] == "result" {
                break frame;
            }
            if frame["type"] == "control_request" {
                let response = match frame["request"]["subtype"].as_str().unwrap() {
                    "mcp_message" => {
                        let mut response = mcp_response(&frame, "");
                        if frame["request"]["message"]["method"] == "tools/list"
                            && frame["request"]["message"]["params"]
                                .get("cursor")
                                .is_none()
                        {
                            response["mcp_response"]["result"]["tools"][0]["outputSchema"] = json!({"type":"object","properties":{"server":{"type":"string"},"text":{"type":"string"}},"required":["server","text"]});
                        }
                        if frame["request"]["message"]["method"] == "tools/call" {
                            response["mcp_response"]["result"]["structuredContent"]["padding"] =
                                json!("RAW_PADDING_MARKER".repeat(16000));
                        }
                        response
                    }
                    "can_use_tool" => json!({"behavior":"allow"}),
                    "hook_callback" => {
                        let input = &frame["request"]["input"];
                        let name = input["tool_name"].as_str().unwrap();
                        if name == "mcp__first__echo" {
                            assert_eq!(
                                serde_json::from_str::<Value>(
                                    input["tool_response"].as_str().unwrap()
                                )
                                .unwrap()["text"],
                                "HOST_first_RAW_PRIVATE"
                            );
                            json!({"hookSpecificOutput":{"hookEventName":"PostToolUse","updatedMCPToolOutput":replacement(),"additionalContext":"POST_CONTEXT"}})
                        } else if name == "mcp__native__stdio_echo" {
                            assert_eq!(input["tool_response"][0]["text"], "STDIO_RESULT");
                            json!({})
                        } else {
                            json!({"hookSpecificOutput":{"hookEventName":"PostToolUse","updatedMCPToolOutput":"MUST_NOT_REPLACE_NATIVE_OR_RESOURCE"}})
                        }
                    }
                    other => panic!("unexpected request {other}"),
                };
                answer(&mut client, &frame, response).await;
            }
            frames.push(frame);
        };
        assert_eq!(result["is_error"], false, "{result}");
        let session = result["session_id"].as_str().unwrap().to_owned();
        let completed: Vec<_> = frames
            .iter()
            .filter(|f| f["type"] == "tool_result")
            .collect();
        assert_eq!(completed.len(), 4, "{completed:?}");
        let sdk = completed
            .iter()
            .find(|f| f["name"] == "mcp__first__echo")
            .unwrap();
        let output = sdk["output"].as_str().unwrap();
        let mut expected = replacement();
        expected
            .as_array_mut()
            .unwrap()
            .push(json!({"type":"text","text":"PostToolUse context: POST_CONTEXT"}));
        assert_eq!(serde_json::from_str::<Value>(output).unwrap(), expected);
        assert!(!completed.iter().any(|f| f
            .to_string()
            .contains("MUST_NOT_REPLACE_NATIVE_OR_RESOURCE")));
        assert!(completed.iter().any(|f| f["output"]
            .as_str()
            .is_some_and(|s| s.contains("COMMAND_REDACTED"))));
        let requests = server.fixture.requests.lock().unwrap().clone();
        let last = requests.last().unwrap();
        let transcript = last["messages"].to_string();
        for marker in [
            "SDK_REDACTED",
            "COMMAND_REDACTED",
            "SDK_READ_SENTINEL",
            "SDK_RESOURCE_TEXT",
            PNG,
        ] {
            assert!(transcript.contains(marker), "{marker}: {transcript}");
        }
        assert!(!transcript.contains("HOST_first_RAW_PRIVATE"));
        assert!(!transcript.contains("RAW_PADDING_MARKER"));
        assert!(!transcript.contains("STDIO_RESULT"));
        if let Ok(files) = std::fs::read_dir(root.path().join("data/tool-outputs")) {
            for file in files {
                assert!(!std::fs::read_to_string(file.unwrap().path())
                    .unwrap()
                    .contains("RAW_PADDING_MARKER"));
            }
        }
        if anthropic {
            assert!(last["messages"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|m| m["content"].as_array())
                .flatten()
                .any(|b| b["type"] == "tool_result"
                    && b["content"]
                        .as_array()
                        .is_some_and(|c| c.iter().any(|i| i["type"] == "image"))));
        } else {
            assert!(last["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["role"] == "user"
                    && m["content"]
                        .as_array()
                        .is_some_and(|c| c.iter().any(|p| p["type"] == "image_url"))));
        }
        assert!(client.finish().await.0.success());
        let paths = deepseek_code_core::runtime::RuntimePaths::new(root.path().join("data")).unwrap();
        let store = deepseek_code_core::runtime::SessionStore::new(paths.open_database().unwrap());
        let (saved_id, saved) = store
            .db
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT t.id,t.output FROM tool_calls t JOIN messages m ON m.id=t.message_id WHERE m.conversation_id=?1 AND t.name='mcp__first__echo'",
                    [&session],
                    |row| Ok((row.get::<_, String>(0)?,row.get::<_, String>(1)?)),
                )
            })
            .unwrap()
            .unwrap();
        assert_eq!(saved, output);
        let metadata=store.db.with_conn(|conn|conn.query_row("SELECT metadata_json FROM mcp_tool_outputs WHERE conversation_id=?1 AND tool_use_id=?2",[&session,&saved_id],|row|row.get::<_,String>(0))).unwrap().unwrap();
        assert!(
            metadata.contains("HOST_first_RAW_PRIVATE"),
            "transport metadata remains separately preserved"
        );
        let fork = store.fork(&session).unwrap();
        let format = if anthropic {
            deepseek_code_core::native_engine::provider_manager::ApiFormat::Anthropic
        } else {
            deepseek_code_core::native_engine::provider_manager::ApiFormat::OpenAI
        };
        let fork_history = store.model_history(&fork, &format).unwrap();
        assert!(serde_json::to_string(&fork_history).unwrap().contains(PNG));
        let resumed = start_server(anthropic, vec![]).await;
        let mut client = Client::start(root.path(), &resumed, &["--resume", &session]);
        client
            .send(user("Continue from the prior tool results"))
            .await;
        let resumed_result = client.result().await;
        assert_eq!(resumed_result["is_error"], false, "{resumed_result}");
        let resumed_request = resumed.fixture.requests.lock().unwrap()[0].clone();
        assert!(resumed_request["messages"].to_string().contains(PNG));
        assert!(!resumed_request["messages"]
            .to_string()
            .contains("HOST_first_RAW_PRIVATE"));
        assert!(client.finish().await.0.success());
    }
}

async fn batch_respond(State(fixture): State<Fixture>, Json(body): Json<Value>) -> Response {
    let first = {
        let mut requests = fixture.requests.lock().unwrap();
        let first = requests.is_empty();
        requests.push(body);
        first
    };
    let text = if fixture.anthropic {
        let mut frames = vec![
            json!({"type":"message_start","message":{"id":"batch","type":"message","role":"assistant","model":"fixture","content":[],"usage":{"input_tokens":10,"output_tokens":0}}}),
        ];
        if first {
            for (index, name) in ["mcp__first__echo", "mcp__second__echo"].iter().enumerate() {
                frames.push(json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":format!("batch-{index}"),"name":name,"input":{}}}));
                frames.push(json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":"{\"value\":\"BATCH\"}"}}));
                frames.push(json!({"type":"content_block_stop","index":index}));
            }
        } else {
            frames.extend([json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"BATCH_DONE"}}),json!({"type":"content_block_stop","index":0})]);
        }
        frames.extend([json!({"type":"message_delta","delta":{"stop_reason":if first {"tool_use"}else{"end_turn"}},"usage":{"output_tokens":4}}),json!({"type":"message_stop"})]);
        frames
            .into_iter()
            .map(|frame| {
                format!(
                    "event: {}\ndata: {frame}\n\n",
                    frame["type"].as_str().unwrap()
                )
            })
            .collect()
    } else {
        let delta = if first {
            json!({"tool_calls":[{"index":0,"id":"batch-0","type":"function","function":{"name":"mcp__first__echo","arguments":"{\"value\":\"BATCH\"}"}},{"index":1,"id":"batch-1","type":"function","function":{"name":"mcp__second__echo","arguments":"{\"value\":\"BATCH\"}"}}]})
        } else {
            json!({"content":"BATCH_DONE"})
        };
        format!(
            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            json!({"choices":[{"index":0,"delta":delta,"finish_reason":null}]}),
            json!({"choices":[{"index":0,"delta":{},"finish_reason":if first {"tool_calls"}else{"stop"}}]})
        )
    };
    ([("content-type", "text/event-stream")], text).into_response()
}

#[tokio::test]
async fn mcp_batches_keep_ids_images_errors_and_hook_serialization() {
    for anthropic in [false, true] {
        for hooks in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let fixture = Fixture {
                anthropic,
                mode: Mode::Text,
                requests: Arc::new(Mutex::new(Vec::new())),
            };
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let app = Router::new()
                .route(
                    if anthropic {
                        "/v1/messages"
                    } else {
                        "/v1/chat/completions"
                    },
                    post(batch_respond),
                )
                .with_state(fixture.clone());
            let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let server = Server { url, fixture, task };
            vision_fixture(root.path(), &server);
            let mut client = Client::start(root.path(), &server, &["--permission-mode", "bypass"]);
            initialize(
                &mut client,
                json!(["first", "second"]),
                if hooks {
                    json!({"PostToolUse":[{"hookCallbackIds":["post"]}]})
                } else {
                    json!({})
                },
            )
            .await;
            client.send(user("Call both MCP tools in one batch")).await;
            let mut results = Vec::new();
            let mut post_count = 0;
            loop {
                let frame = next(&mut client).await;
                if frame["type"] == "result" {
                    assert_eq!(frame["is_error"], false);
                    break;
                }
                if frame["type"] == "tool_result" {
                    results.push(frame.clone());
                }
                if frame["type"] != "control_request" {
                    continue;
                }
                let response = if frame["request"]["subtype"] == "hook_callback" {
                    post_count += 1;
                    let id = frame["request"]["input"]["tool_use_id"].as_str().unwrap();
                    json!({"hookSpecificOutput":{"hookEventName":"PostToolUse","updatedMCPToolOutput":format!("REPLACED_{id}")}})
                } else {
                    let mut response = mcp_response(&frame, "");
                    if frame["request"]["message"]["method"] == "tools/call" {
                        let first = frame["request"]["server_name"] == "first";
                        if hooks && !first {
                            assert_eq!(
                                post_count, 1,
                                "hooked batch must not bypass the serial wrapper"
                            );
                        }
                        response["mcp_response"]["result"] = if first {
                            json!({"content":[{"type":"image","mimeType":"image/png","data":PNG}],"isError":false})
                        } else {
                            json!({"content":[{"type":"text","text":"SECOND_RESULT"}],"isError":!hooks})
                        };
                    }
                    response
                };
                answer(&mut client, &frame, response).await;
            }
            assert_eq!(post_count, if hooks { 2 } else { 0 });
            assert_eq!(results.len(), 2);
            let first = results.iter().find(|r| r["id"] == "batch-0").unwrap();
            let second = results.iter().find(|r| r["id"] == "batch-1").unwrap();
            assert_eq!(first["is_error"], false);
            assert_eq!(second["is_error"], !hooks);
            let requests = server.fixture.requests.lock().unwrap();
            let messages = requests[1]["messages"].as_array().unwrap();
            if hooks {
                assert_eq!(first["output"], "REPLACED_batch-0");
                assert_eq!(second["output"], "REPLACED_batch-1");
                assert!(!requests[1].to_string().contains(PNG));
            } else {
                assert!(requests[1].to_string().contains(PNG));
                if !anthropic {
                    let last_tool = messages.iter().rposition(|m| m["role"] == "tool").unwrap();
                    let image = messages
                        .iter()
                        .position(|m| m["role"] == "user" && m["content"].to_string().contains(PNG))
                        .unwrap();
                    assert!(
                        image > last_tool,
                        "all tool results must precede image continuation"
                    );
                }
            }
            drop(requests);
            assert!(client.finish().await.0.success());
        }
    }
}

#[tokio::test]
async fn failed_mcp_calls_use_failure_hooks_and_keep_error_status() {
    for anthropic in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let server = start_server(
            anthropic,
            vec![("mcp__first__echo", json!({"value":"ERROR"}))],
        )
        .await;
        let mut client = Client::start(root.path(), &server, &[]);
        initialize(&mut client,json!(["first"]),json!({"PostToolUse":[{"hookCallbackIds":["must-not-run"]}],"PostToolUseFailure":[{"hookCallbackIds":["failed"]}]})).await;
        client.send(user("Call the failing MCP tool")).await;
        let (result, frames) = drive(&mut client, "is-error").await;
        assert_eq!(result["is_error"], false);
        assert!(frames
            .iter()
            .any(|f| f["type"] == "tool_result" && f["is_error"] == true));
        let callbacks: Vec<_> = frames
            .iter()
            .filter(|f| f["request"]["subtype"] == "hook_callback")
            .collect();
        assert_eq!(callbacks.len(), 1);
        assert_eq!(callbacks[0]["request"]["callback_id"], "failed");
        assert!(client.finish().await.0.success());
    }
}

#[tokio::test]
async fn pending_post_mcp_output_cancels_or_times_out_without_late_replacement() {
    for anthropic in [false, true] {
        for mode in ["interrupt", "eof", "timeout"] {
            let root = tempfile::tempdir().unwrap();
            let server = start_server(
                anthropic,
                vec![("mcp__first__echo", json!({"value":"ORIGINAL"}))],
            )
            .await;
            let mut client = Client::start(root.path(), &server, &[]);
            initialize(&mut client,json!(["first"]),json!({"PostToolUse":[{"hookCallbackIds":["post"],"timeout":if mode=="timeout" {0.1}else{30.0}}]})).await;
            client
                .send(user("Call MCP and wait for the post hook"))
                .await;
            let callback = loop {
                let frame = next(&mut client).await;
                assert_ne!(frame["type"], "result");
                if frame["type"] == "control_request" {
                    if frame["request"]["subtype"] == "hook_callback" {
                        break frame;
                    }
                    let response = if frame["request"]["subtype"] == "mcp_message" {
                        mcp_response(&frame, "")
                    } else {
                        json!({"behavior":"allow"})
                    };
                    answer(&mut client, &frame, response).await;
                }
            };
            if mode == "interrupt" {
                client.send(json!({"type":"control_request","request_id":"cancel-post","request":{"subtype":"interrupt"}})).await;
                let result = client.result().await;
                assert_eq!(result["is_error"], true);
                answer(&mut client,&callback,json!({"hookSpecificOutput":{"hookEventName":"PostToolUse","updatedMCPToolOutput":"LATE_SECRET"}})).await;
            } else if mode == "timeout" {
                let result = client.result().await;
                assert_eq!(result["is_error"], false);
                answer(&mut client,&callback,json!({"hookSpecificOutput":{"hookEventName":"PostToolUse","updatedMCPToolOutput":"LATE_SECRET"}})).await;
            }
            let (_, frames) = client.finish().await;
            assert!(!frames.iter().any(|f| f.to_string().contains("LATE_SECRET")));
            let requests = server.fixture.requests.lock().unwrap();
            assert_eq!(requests.len(), if mode == "timeout" { 2 } else { 1 });
            assert!(!requests
                .iter()
                .any(|r| r.to_string().contains("LATE_SECRET")));
        }
    }
}
