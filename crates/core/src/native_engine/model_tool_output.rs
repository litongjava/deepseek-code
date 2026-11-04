//! Model-only rendering. Stored results, hooks and UI events keep their original data.
use serde_json::Value;

impl super::ToolLoopExecutor {
    /// Called by both provider builders, after typed MCP/image restoration.
    pub(super) fn prepare_history_tool_outputs(&self, messages: &mut [Value]) {
        let mut names = std::collections::HashMap::<String, String>::new();
        for message in messages {
            if let Some(calls) = message["tool_calls"].as_array() {
                for call in calls {
                    if let (Some(id), Some(name)) =
                        (call["id"].as_str(), call["function"]["name"].as_str())
                    {
                        names.insert(id.into(), name.into());
                    }
                }
            }
            if message["role"] == "tool" {
                if let (Some(name), Some(text)) = (
                    message["tool_call_id"]
                        .as_str()
                        .and_then(|id| names.get(id)),
                    message["content"].as_str(),
                ) {
                    message["content"] = Value::String(self.preview_tool_output(name, text));
                }
            } else if let Some(blocks) = message["content"].as_array_mut() {
                for block in blocks {
                    if block["type"] == "tool_use" {
                        if let (Some(id), Some(name)) =
                            (block["id"].as_str(), block["name"].as_str())
                        {
                            names.insert(id.into(), name.into());
                        }
                    } else if block["type"] == "tool_result" {
                        if let Some(name) =
                            block["tool_use_id"].as_str().and_then(|id| names.get(id))
                        {
                            self.retain_content(name, &mut block["content"]);
                        }
                    }
                }
            }
        }
    }

    pub(super) fn retain_content(&self, name: &str, content: &mut Value) {
        if let Some(text) = content.as_str() {
            *content = Value::String(self.preview_tool_output(name, text));
        } else if let Some(blocks) = content.as_array() {
            // Text-only MCP blocks share one budget. Images and opaque blocks stay intact.
            if blocks.iter().all(|block| {
                block["type"] == "text"
                    && block["text"].is_string()
                    && block.as_object().is_some_and(|o| o.len() == 2)
            }) {
                let text = blocks
                    .iter()
                    .filter_map(|block| block["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                *content = Value::String(self.preview_tool_output(name, &text));
            }
        }
    }
}

pub(super) fn render(name: &str, output: &str, body_budget: Option<usize>) -> String {
    if !matches!(name, "Bash" | "Read" | "Write" | "Edit" | "MultiEdit") {
        return output.into();
    }
    let (prefix, json) = output
        .strip_prefix("Error: ")
        .map_or(("", output), |s| ("Error: ", s));
    // Hooks can append context after the JSON value; preserve that suffix verbatim.
    let mut stream = serde_json::Deserializer::from_str(json).into_iter::<Value>();
    let Some(Ok(Value::Object(mut fields))) = stream.next() else {
        return output.into();
    };
    let suffix = &json[stream.byte_offset()..];
    if matches!(name, "Write" | "Edit" | "MultiEdit") {
        if fields.remove("file_change").is_none() {
            return output.into();
        }
        return format!("{prefix}{}{suffix}", Value::Object(fields));
    }
    let mut parts = Vec::new();
    let mut bodies = Vec::new();
    if name == "Read" {
        let Some(content) = fields
            .get("content")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            return output.into();
        };
        // Images are handled by the multimodal path, never flattened here.
        if fields.contains_key("image") {
            return output.into();
        }
        fields.remove("content");
        if let Some(lines) = fields.remove("lines") {
            if fields.get("truncated") == Some(&Value::Bool(true)) {
                parts.push(format!(
                    "File has {lines} lines; more lines remain. Use offset/limit to continue."
                ));
            }
        }
        fields.remove("truncated");
        bodies.push(content);
    } else {
        if !fields.contains_key("stdout")
            && !fields.contains_key("stderr")
            && !fields.contains_key("error")
        {
            return output.into();
        }
        if let Some(code) = fields.remove("exit_code") {
            parts.push(format!("Exit code: {code}"));
        }
        if fields.get("success") == Some(&Value::Bool(false)) && parts.is_empty() {
            parts.push("Command failed".into());
        }
        fields.remove("success");
        fields.remove("shell");
        for (key, label) in [
            ("timed_out", "Command timed out"),
            ("is_error", "Tool reported an error"),
            (
                "stdout_truncated",
                "Stdout is incomplete (capture or tool limit)",
            ),
            (
                "stderr_truncated",
                "Stderr is incomplete (capture or tool limit)",
            ),
        ] {
            if fields.remove(key) == Some(Value::Bool(true)) {
                parts.push(label.into());
            }
        }
        if let Some(error) = fields.remove("error") {
            parts.push(format!(
                "Error: {}",
                error
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| error.to_string())
            ));
        }
        let streams = ["stdout", "stderr"]
            .iter()
            .filter(|key| {
                fields
                    .get(**key)
                    .and_then(Value::as_str)
                    .is_some_and(|s| !s.is_empty())
            })
            .count()
            .max(1);
        for (key, label) in [("stdout", "Output"), ("stderr", "Stderr")] {
            if let Some(Value::String(text)) = fields.remove(key) {
                if !text.is_empty() {
                    let text = body_budget.map_or_else(
                        || text.clone(),
                        |budget| crate::performance::head_tail_preview(&text, budget / streams),
                    );
                    bodies.push(format!("{label}:\n{text}"));
                }
            }
        }
    }
    // Preserve extensions (diagnostics, paths, task IDs) rather than dropping unknown fields.
    if !fields.is_empty() {
        parts.push(format!("Metadata: {}", Value::Object(fields)));
    }
    parts.extend(bodies);
    format!("{prefix}{}{suffix}", parts.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::*;

    #[test]
    fn model_tool_output_preserves_payload_errors_and_hook_suffix() {
        let raw = json!({"stdout":"{\"user_json\": 1}\n", "stderr":"", "exit_code":0,"success":true,"shell":"bash","stdout_truncated":false,"stderr_truncated":false}).to_string();
        assert_eq!(
            render("Bash", &raw, None),
            "Exit code: 0\nOutput:\n{\"user_json\": 1}\n"
        );
        assert_eq!(render("mcp__fixture__Bash", &raw, None), raw);
        let error = format!(
            "Error: {}\nPostToolUseFailure context: inspect dependency",
            json!({"stdout":"partial","stderr":"last error","exit_code":7,"success":false,"timed_out":true,"stdout_truncated":true,"task_id":"task-1"})
        );
        let rendered = render("Bash", &error, None);
        for expected in [
            "Exit code: 7",
            "Command timed out",
            "Stdout is incomplete",
            "last error",
            "task-1",
            "PostToolUseFailure context: inspect dependency",
        ] {
            assert!(rendered.contains(expected), "{rendered}");
        }
        assert_eq!(
            render("Bash", "Error: Starting shell command", None),
            "Error: Starting shell command"
        );
        let read = json!({"content":"   51\tcode", "lines":100,"truncated":true,"lsp_diagnostics":{"error":"missing name"}}).to_string();
        let rendered = render("Read", &read, None);
        assert!(rendered.contains("more lines remain"));
        assert!(rendered.contains("missing name"));
        assert!(rendered.ends_with("   51\tcode"));
    }

    fn executor(root: &std::path::Path) -> ToolLoopExecutor {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let provider = ResolvedProvider {
            provider: serde_json::from_value(json!({"id":"fixture","name":"fixture","base_url":"http://127.0.0.1:1","api_key":"unused","api_format":"openai","models":[],"enabled":true})).unwrap(),
            model: serde_json::from_value(json!({"id":"fixture","name":"fixture","enabled":true,"supports_vision":false,"supports_web_search":false})).unwrap(),
        };
        let (tx, _rx) = mpsc::channel(16);
        ToolLoopExecutor::new(
            provider,
            vec![],
            None,
            128,
            tx,
            root.to_string_lossy().into(),
            ReasoningMode::Standard,
        )
        .with_search_config_path(root.join("config.json"))
    }

    #[test]
    fn model_tool_output_replay_matches_fresh_for_both_protocols_and_forks() {
        let temp = tempfile::tempdir().unwrap();
        let paths = crate::runtime::RuntimePaths::new(temp.path().join("data")).unwrap();
        let store = crate::runtime::SessionStore::new(paths.open_database().unwrap());
        let id = store.create(temp.path(), "fixture", "fixture").unwrap();
        let assistant = store.append(&id, "assistant", "").unwrap();
        let raw = format!(
            "Error: {}\nHook context: keep this",
            json!({"stdout":format!("START\n{}\nTEST SUMMARY", "中🙂".repeat(30_000)),"stderr":"FINAL ERROR","exit_code":7,"success":false})
        );
        store
            .db
            .with_conn(|conn| {
                crate::db::message_repo::insert_tool_call(
                    conn,
                    "call",
                    &assistant,
                    "Bash",
                    Some("{}"),
                    Some(&raw),
                    true,
                    0,
                )
            })
            .unwrap()
            .unwrap();
        let mut engine = executor(temp.path());
        let fresh = engine.retained_tool_output("Bash", &raw);
        assert!(fresh.len() <= 16_000);
        for expected in [
            "Exit code: 7",
            "START",
            "TEST SUMMARY",
            "FINAL ERROR",
            "Hook context: keep this",
            "Saved tool result",
        ] {
            assert!(fresh.contains(expected), "missing {expected}");
        }
        let fork = store.fork(&id).unwrap();
        for session in [&id, &fork] {
            for format in [ApiFormat::OpenAI, ApiFormat::Anthropic] {
                engine.messages = store.model_history(session, &format).unwrap();
                let built = match format {
                    ApiFormat::OpenAI => {
                        serde_json::to_value(engine.build_openai_messages().unwrap()).unwrap()
                    }
                    ApiFormat::Anthropic => {
                        serde_json::to_value(engine.build_anthropic_messages().unwrap()).unwrap()
                    }
                };
                let actual = if matches!(format, ApiFormat::OpenAI) {
                    &built[1]["content"]
                } else {
                    &built[1]["content"][0]["content"]
                };
                assert_eq!(actual, &json!(fresh));
                assert_eq!(engine.messages[1]["content"].as_str().unwrap_or(&raw), raw);
            }
        }
        assert_eq!(
            std::fs::read_dir(temp.path().join("tool-outputs"))
                .unwrap()
                .count(),
            1
        );
        let stored = store
            .db
            .with_conn(|conn| crate::db::message_repo::get_tool_calls_for_message(conn, &assistant))
            .unwrap()
            .unwrap();
        assert_eq!(stored[0].output.as_deref(), Some(raw.as_str()));
    }

    #[test]
    fn model_tool_output_mcp_text_budget_preserves_images_and_opaque_blocks() {
        let temp = tempfile::tempdir().unwrap();
        let engine = executor(temp.path());
        let mut content =
            json!([{"type":"text","text":"头".repeat(20_000)},{"type":"text","text":"TAIL"}]);
        engine.retain_content("mcp__fixture__logs", &mut content);
        assert!(content.as_str().unwrap().len() <= 16_000);
        assert!(content.as_str().unwrap().contains("TAIL"));
        for original in [
            json!([{"type":"image","source":{"data":"abc"}},{"type":"text","text":"caption"}]),
            json!([{"type":"text","text":"signed","signature":"opaque"}]),
        ] {
            let mut content = original.clone();
            engine.retain_content("mcp__fixture__logs", &mut content);
            assert_eq!(content, original);
        }
    }

    #[tokio::test]
    async fn model_tool_output_reaches_actual_provider_requests() {
        use axum::{http::header, routing::post, Json, Router};
        for format in ["openai", "anthropic"] {
            let temp = tempfile::tempdir().unwrap();
            let source = temp.path().join("source.txt");
            std::fs::write(&source, "source line\n").unwrap();
            let calls = vec![
                (
                    "shell",
                    "Bash",
                    json!({"command":"node -e \"process.stdout.write('START'+ 'x'.repeat(200000)+'TEST SUMMARY');process.stderr.write('FINAL ERROR');process.exitCode=7\""}),
                ),
                ("read", "Read", json!({"file_path":source})),
            ];
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base = format!("http://{}", listener.local_addr().unwrap());
            let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
            let requests = captured.clone();
            let app = Router::new().route(if format == "anthropic" { "/v1/messages" } else { "/v1/chat/completions" }, post(move |Json(body): Json<Value>| {
                let requests = requests.clone();
                let calls = calls.clone();
                async move {
                    let mut requests = requests.lock().await;
                    let first = requests.is_empty();
                    requests.push(body);
                    let mut events = Vec::new();
                    if format == "anthropic" {
                        events.push(json!({"type":"message_start","message":{"id":"fixture","model":"fixture","usage":{"input_tokens":1}}}));
                        if first {
                            for (index, (id, name, input)) in calls.iter().enumerate() {
                                events.push(json!({"type":"content_block_start","index":index,"content_block":{"type":"tool_use","id":id,"name":name,"input":{}}}));
                                events.push(json!({"type":"content_block_delta","index":index,"delta":{"type":"input_json_delta","partial_json":input.to_string()}}));
                                events.push(json!({"type":"content_block_stop","index":index}));
                            }
                        } else {
                            events.push(json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}));
                            events.push(json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"done"}}));
                            events.push(json!({"type":"content_block_stop","index":0}));
                        }
                        events.push(json!({"type":"message_delta","delta":{"stop_reason":if first {"tool_use"} else {"end_turn"}},"usage":{"output_tokens":1}}));
                        events.push(json!({"type":"message_stop"}));
                    } else if first {
                        let calls: Vec<_> = calls.iter().enumerate().map(|(index, (id, name, input))| json!({"index":index,"id":id,"type":"function","function":{"name":name,"arguments":input.to_string()}})).collect();
                        events.push(json!({"id":"fixture","model":"fixture","choices":[{"index":0,"delta":{"tool_calls":calls},"finish_reason":null}]}));
                        events.push(json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}));
                    } else {
                        events.push(json!({"id":"fixture","model":"fixture","choices":[{"index":0,"delta":{"content":"done"},"finish_reason":null}]}));
                        events.push(json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}));
                    }
                    let mut stream = events.into_iter().map(|event| format!("data: {event}\n\n")).collect::<String>();
                    if format == "openai" { stream.push_str("data: [DONE]\n\n"); }
                    ([(header::CONTENT_TYPE, "text/event-stream")], stream)
                }
            }));
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let mut engine = executor(temp.path());
            let (tx, _events) = mpsc::channel(128);
            engine.event_tx = tx;
            engine.provider.provider.base_url = base;
            engine.provider.provider.api_format = if format == "anthropic" {
                ApiFormat::Anthropic
            } else {
                ApiFormat::OpenAI
            };
            engine.messages = vec![json!({"role":"user","content":"run fixture"})];
            let result = tokio::time::timeout(Duration::from_secs(20), engine.execute()).await;
            server.abort();
            assert_eq!(result.unwrap().unwrap().0, "done");
            let requests = captured.lock().await;
            assert_eq!(requests.len(), 2);
            let mut outputs = Vec::new();
            for message in requests[1]["messages"].as_array().unwrap() {
                if message["role"] == "tool" {
                    outputs.push(message["content"].as_str().unwrap());
                }
                if let Some(blocks) = message["content"].as_array() {
                    for block in blocks {
                        if block["type"] == "tool_result" {
                            outputs.push(block["content"].as_str().unwrap());
                        }
                    }
                }
            }
            assert_eq!(outputs.len(), 2);
            for expected in [
                "Exit code: 7",
                "START",
                "TEST SUMMARY",
                "FINAL ERROR",
                "Stdout is incomplete",
            ] {
                assert!(
                    outputs[0].contains(expected),
                    "{format}: missing {expected}: {}",
                    outputs[0]
                );
            }
            assert!(outputs[0].len() <= 16_000);
            assert_eq!(outputs[1], "     1\tsource line");
        }
    }
}
