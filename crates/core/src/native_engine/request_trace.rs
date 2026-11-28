//! One model response per request. Tool execution time is deliberately excluded.
use serde_json::{json, Value};
use std::{collections::BTreeMap, time::Instant};

pub struct RequestTrace {
    context_capture: Option<super::request_context::ContextCapture>,
    snapshot: Value,
    clock: Instant,
    first_content_ms: Option<u64>,
    blocks: Vec<Value>,
    anthropic_indices: BTreeMap<u64, usize>,
    tool_indices: BTreeMap<u64, usize>,
    arguments: BTreeMap<usize, String>,
    usage: Value,
    finish_reason: Option<String>,
    response_id: Option<String>,
    response_finished: bool,
    /// A `finish_reason` arrived on some choice, which is as terminal as `[DONE]`.
    terminal_observed: bool,
    /// A provider error event, already reduced to the shared vocabulary.
    remote_failure: Option<crate::performance::RequestFailure>,
    /// Recovery-only: any shape of provider error event was seen.
    openai_provider_error: bool,
    /// Recovery-only: content on a choice the first-choice assembly may skip.
    openai_recovery_content: bool,
    /// Cross-turn opaque Responses reasoning items with `encrypted_content`.
    provider_reasoning: Option<Value>,
}

pub(super) fn cache_write_tokens(usage: &Value) -> Option<u64> {
    usage["cache_creation_input_tokens"]
        .as_u64()
        .or_else(|| usage["prompt_tokens_details"]["cache_write_tokens"].as_u64())
}

impl RequestTrace {
    pub fn new(mut snapshot: Value) -> Self {
        super::request_identity::annotate(&mut snapshot);
        snapshot["status"] = json!("running");
        snapshot["responseBlocks"] = json!([]);
        Self {
            context_capture: None,
            snapshot,
            clock: Instant::now(),
            first_content_ms: None,
            blocks: vec![],
            anthropic_indices: BTreeMap::new(),
            tool_indices: BTreeMap::new(),
            arguments: BTreeMap::new(),
            usage: json!({}),
            finish_reason: None,
            response_id: None,
            response_finished: false,
            terminal_observed: false,
            remote_failure: None,
            openai_provider_error: false,
            openai_recovery_content: false,
            provider_reasoning: None,
        }
    }

    pub fn set_provider_reasoning(&mut self, native: &Value) {
        let Some(provider) = native["provider"].as_str().filter(|s| !s.is_empty()) else {
            return;
        };
        let Some(model) = native["model"].as_str().filter(|s| !s.is_empty()) else {
            return;
        };
        if native["api"] != "openai-responses" {
            return;
        }
        let Some(items) = native["items"].as_array() else {
            return;
        };
        let encrypted: Vec<Value> = items
            .iter()
            .filter(|item| {
                item["type"] == "reasoning"
                    && item["encrypted_content"]
                        .as_str()
                        .is_some_and(|s| !s.trim().is_empty())
            })
            .cloned()
            .collect();
        if encrypted.is_empty() {
            return;
        }
        self.provider_reasoning = Some(json!({
            "api": "openai-responses",
            "provider": provider,
            "model": model,
            "reasoning_only": true,
            "items": encrypted,
        }));
    }

    pub fn capture_context(
        &mut self,
        sender: tokio::sync::mpsc::Sender<super::tool_loop::EngineEvent>,
    ) -> super::request_context::ContextCapture {
        let capture = super::request_context::ContextCapture {
            event: Some((sender, self.started())),
            ..Default::default()
        };
        self.context_capture = Some(capture.clone());
        capture
    }

    pub fn started(&self) -> Value {
        let mut snapshot = self.snapshot.clone();
        snapshot["usage"] = self.usage.clone();
        if let Some(usage) = self.context_capture.as_ref().and_then(|c| c.snapshot()) {
            snapshot["contextUsage"] = serde_json::to_value(usage).unwrap_or(Value::Null);
        }
        snapshot
    }

    pub fn mark_completed(&mut self) {
        self.response_finished = true;
        self.terminal_observed = true;
    }

    /// Recovery-only view of this response.
    ///
    /// A replay is safe only while the provider had not finished: a raw
    /// `[DONE]`, a `finish_reason` on *any* choice (a non-leading choice can
    /// carry `tool_calls`), or a provider SSE error event all mean the model
    /// already produced its answer, so a later body timeout is not a stall.
    /// This does not alter the ordinary completion or failure outcome.
    pub fn post_content_recovery_blocked(&self) -> bool {
        self.response_finished
            || self.terminal_observed
            || self.remote_failure.is_some()
            || self.openai_provider_error
    }

    /// Recovery-only view of how much of the response was already produced.
    ///
    /// Direct Chat can deliver content on a choice the first-choice assembly
    /// skips, so this flag counts the deltas the tool loop actually consumes.
    /// The adapter's content-start marker remains content too.
    pub fn openai_recovery_content_observed(&self) -> bool {
        self.openai_recovery_content || self.first_content_observed()
    }

    /// A provider error event is remote data: it is reduced here, before the
    /// snapshot, the emitted event or the stored session can see it.
    fn observe_error(&mut self, event: &Value) {
        let Some(error) = event.get("error").filter(|error| !error.is_null()) else {
            return;
        };
        let content_started = !self.blocks.is_empty();
        self.remote_failure = Some(crate::performance::RequestFailure::remote_event(
            error,
            None,
            None,
            content_started,
        ));
    }

    fn first_content(&mut self) {
        self.first_content_ms
            .get_or_insert(self.clock.elapsed().as_millis() as u64);
    }

    /// Whether the response being observed has exposed content of any kind:
    /// text, reasoning, a tool call, or an adapter's content-start marker.
    /// Turn-wide text and assembled tool calls are not part of this decision.
    pub fn first_content_observed(&self) -> bool {
        self.first_content_ms.is_some()
    }

    pub(super) fn usage(&self) -> &Value {
        &self.usage
    }

    fn merge_usage(&mut self, usage: &Value) {
        fn update(current: &mut Value, reported: &Value) {
            if let Some(fields) = reported.as_object() {
                if !current.is_object() {
                    *current = json!({});
                }
                for (key, value) in fields {
                    if !value.is_null() {
                        update(&mut current[key], value);
                    }
                }
            } else if !reported.is_null() {
                // Reports within one request are cumulative, never additive.
                *current = reported.clone();
            }
        }
        update(&mut self.usage, usage);
    }

    fn append_text(&mut self, kind: &str, text: &str) {
        if text.is_empty() {
            return;
        }
        self.first_content();
        if let Some(last) = self.blocks.last_mut().filter(|block| block["type"] == kind) {
            let combined = format!("{}{}", last["text"].as_str().unwrap_or_default(), text);
            last["text"] = json!(combined);
        } else {
            self.blocks.push(json!({"type": kind, "text": text}));
        }
    }

    pub fn observe_openai(&mut self, event: &Value) {
        self.observe_error(event);
        // Both provider error shapes are terminal for recovery, even when a
        // later read of the still-open body genuinely times out.
        if super::provider_runtime::chat_error(event).is_some() {
            self.openai_provider_error = true;
        }
        if event["response_content_started"] == true {
            self.first_content();
        }
        if let Some(id) = event["id"].as_str() {
            self.response_id = Some(id.to_string());
        }
        if let Some(model) = event["model"].as_str() {
            self.snapshot["responseModel"] = json!(model);
        }
        if let Some(usage) = event.get("usage") {
            self.merge_usage(usage);
        }
        // Built-in adapters return authoritative final block order and signatures.
        if let Some(content) = event["provider_content"]["content"].as_array() {
            self.blocks = content
                .iter()
                .map(|source| {
                    let mut block = source.clone();
                    match source["type"].as_str().unwrap_or_default() {
                        "thinking" => {
                            block["text"] = source["thinking"].clone();
                        }
                        "toolCall" => {
                            block["type"] = json!("tool_call");
                            block["input"] = source["arguments"].clone();
                            block["arguments"] = json!(source["arguments"].to_string());
                        }
                        _ => {}
                    }
                    block
                })
                .collect();
            self.arguments.clear();
        }
        // A finish reason on any choice is terminal, including `tool_calls` on a
        // non-leading choice that the block assembly below does not read.
        if let Some(choices) = event["choices"].as_array() {
            if choices
                .iter()
                .any(|choice| choice["finish_reason"].is_string())
            {
                self.terminal_observed = true;
            }
            // The loop consumes text, thinking and tool deltas from every
            // choice, so recovery content counts them all; role-only and empty
            // deltas stay prelude.
            self.openai_recovery_content |= choices.iter().any(|choice| {
                let delta = &choice["delta"];
                ["content", "reasoning_content"].iter().any(|field| {
                    delta
                        .get(*field)
                        .and_then(|value| value.as_str())
                        .is_some_and(|text| !text.is_empty())
                }) || delta["tool_calls"]
                    .as_array()
                    .is_some_and(|calls| !calls.is_empty())
            });
        }
        let Some(choice) = event["choices"]
            .as_array()
            .and_then(|choices| choices.first())
        else {
            return;
        };
        if let Some(reason) = choice["finish_reason"].as_str() {
            self.finish_reason = Some(reason.to_string());
        }
        let delta = &choice["delta"];
        if let Some(text) = delta["reasoning_content"]
            .as_str()
            .or_else(|| delta["reasoning"].as_str())
        {
            self.append_text("thinking", text);
        }
        if let Some(text) = delta["content"].as_str() {
            self.append_text("text", text);
        }
        if let Some(calls) = delta["tool_calls"].as_array() {
            for call in calls {
                self.first_content();
                let index = call["index"].as_u64().unwrap_or(0);
                let position = *self.tool_indices.entry(index).or_insert_with(|| {
                    self.blocks
                        .push(json!({"type": "tool_call", "id": "", "name": "", "input": {}}));
                    self.blocks.len() - 1
                });
                let block = &mut self.blocks[position];
                if let Some(id) = call["id"].as_str() {
                    block["id"] = json!(id);
                }
                if let Some(delta) = call["function"]["name"]
                    .as_str()
                    .filter(|name| !name.is_empty())
                {
                    let current = block["name"].as_str().unwrap_or_default().to_owned();
                    if !current.ends_with(delta) {
                        block["name"] = json!(format!("{current}{delta}"));
                    }
                }
                if let Some(args) = call["function"]["arguments"].as_str() {
                    self.arguments.entry(position).or_default().push_str(args);
                }
            }
        }
    }

    pub fn observe_anthropic(&mut self, event: &Value) {
        self.observe_error(event);
        match event["type"].as_str().unwrap_or_default() {
            "message_stop" => self.mark_completed(),
            "message_start" => {
                if let Some(id) = event["message"]["id"].as_str() {
                    self.response_id = Some(id.to_string());
                }
                if let Some(model) = event["message"]["model"].as_str() {
                    self.snapshot["responseModel"] = json!(model);
                }
                if let Some(usage) = event["message"].get("usage") {
                    self.merge_usage(usage);
                }
            }
            "content_block_start" => {
                let index = event["index"].as_u64().unwrap_or(0);
                let source = &event["content_block"];
                let block = match source["type"].as_str().unwrap_or_default() {
                    "tool_use" => {
                        self.first_content();
                        json!({"type":"tool_call", "id":source["id"], "name":source["name"], "input":source["input"]})
                    }
                    "thinking" => {
                        let text = source["thinking"].as_str().unwrap_or_default();
                        if !text.is_empty() {
                            self.first_content();
                        }
                        let mut block = source.clone();
                        block["text"] = json!(text);
                        block
                    }
                    "text" => {
                        let text = source["text"].as_str().unwrap_or_default();
                        if !text.is_empty() {
                            self.first_content();
                        }
                        source.clone()
                    }
                    _ => source.clone(),
                };
                self.anthropic_indices.insert(index, self.blocks.len());
                self.blocks.push(block);
            }
            "content_block_delta" => {
                let index = event["index"].as_u64().unwrap_or(0);
                let Some(&position) = self.anthropic_indices.get(&index) else {
                    return;
                };
                let delta = &event["delta"];
                if let Some(text) = delta["text"]
                    .as_str()
                    .or_else(|| delta["thinking"].as_str())
                {
                    if !text.is_empty() {
                        self.first_content();
                    }
                    let combined = format!(
                        "{}{}",
                        self.blocks[position]["text"].as_str().unwrap_or_default(),
                        text
                    );
                    self.blocks[position]["text"] = json!(combined);
                }
                if let Some(args) = delta["partial_json"]
                    .as_str()
                    .filter(|args| !args.is_empty())
                {
                    self.arguments.entry(position).or_default().push_str(args);
                }
                if let Some(signature) = delta["signature"].as_str() {
                    let combined = format!(
                        "{}{}",
                        self.blocks[position]["signature"]
                            .as_str()
                            .unwrap_or_default(),
                        signature
                    );
                    self.blocks[position]["signature"] = json!(combined);
                }
            }
            "message_delta" => {
                if let Some(reason) = event["delta"]["stop_reason"].as_str() {
                    self.finish_reason = Some(reason.to_string());
                }
                if let Some(usage) = event.get("usage") {
                    self.merge_usage(usage);
                }
            }
            _ => {}
        }
    }

    pub fn finish(self, status: &str, error: Option<&str>) -> Value {
        self.finish_observed(status, error, None, None)
    }

    /// Same as [`Self::finish`], with the HTTP attempts and status observed by
    /// the send layer so a provider error event reports them instead of
    /// `unknown`.
    pub fn finish_observed(
        mut self,
        status: &str,
        error: Option<&str>,
        attempts: Option<u32>,
        http_status: Option<u16>,
    ) -> Value {
        let failure = self
            .remote_failure
            .take()
            .map(|failure| failure.with_observation(attempts, http_status));
        let mut error = error
            .map(String::from)
            .or_else(|| failure.as_ref().map(|failure| failure.diagnostic()));
        let status = if error.is_some() {
            "error"
        } else if status == "completed"
            && matches!(self.finish_reason.as_deref(), Some("max_tokens" | "length"))
        {
            error=Some(format!("Incomplete response: provider stopped at the output token limit ({}). Partial output was retained; send a follow-up to continue.",self.finish_reason.as_deref().unwrap()));
            "incomplete"
        } else if status == "completed" && !self.response_finished && self.finish_reason.is_none() {
            error = Some("Model response ended before completion: provider stream closed without a completion marker. Partial output was retained; pending tools were not executed.".into());
            // Distinct from `interrupted`, which means the consumer went away.
            // A stream that ended early is a failure, not a user cancellation.
            "stream_error"
        } else {
            status
        };
        if let Some(failure) = &failure {
            self.snapshot["failure"] = failure.as_json();
        }
        let duration_ms = self.clock.elapsed().as_millis() as u64;
        let generation_ms = self
            .first_content_ms
            .map(|first| duration_ms.saturating_sub(first));
        for (position, args) in &self.arguments {
            self.blocks[*position]["arguments"] = json!(args);
            self.blocks[*position]["input"] =
                serde_json::from_str::<Value>(args).unwrap_or_else(|_| json!(args));
        }
        let input = self.usage["input_tokens"]
            .as_u64()
            .or_else(|| self.usage["prompt_tokens"].as_u64());
        let output = self.usage["output_tokens"]
            .as_u64()
            .or_else(|| self.usage["completion_tokens"].as_u64());
        let reasoning = self.usage["completion_tokens_details"]["reasoning_tokens"]
            .as_u64()
            .or_else(|| self.usage["output_tokens_details"]["reasoning_tokens"].as_u64())
            .or_else(|| self.usage["reasoning_tokens"].as_u64());
        let content = output
            .zip(reasoning)
            .and_then(|(output, reasoning)| output.checked_sub(reasoning));
        let cache_read = self.usage["cache_read_input_tokens"]
            .as_u64()
            .or_else(|| self.usage["prompt_tokens_details"]["cached_tokens"].as_u64())
            .or_else(|| self.usage["prompt_cache_hit_tokens"].as_u64())
            .or_else(|| self.usage["cached_tokens"].as_u64());
        let cache_write = cache_write_tokens(&self.usage);
        // Anthropic input_tokens excludes cached input; OpenAI prompt_tokens includes it.
        let total = self.usage["total_tokens"].as_u64().or_else(|| {
            input.zip(output).map(|(input, output)| {
                input
                    + output
                    + if self.usage.get("input_tokens").is_some() {
                        cache_read.unwrap_or(0) + cache_write.unwrap_or(0)
                    } else {
                        0
                    }
            })
        });
        let throughput = output
            .zip(generation_ms)
            .filter(|(_, ms)| *ms > 0)
            .map(|(tokens, ms)| tokens as f64 * 1000.0 / ms as f64);
        let first_at = self.snapshot["startedAt"]
            .as_u64()
            .zip(self.first_content_ms)
            .map(|(start, delay)| start + delay);
        let fields = json!({"status":status, "endedAt":chrono::Utc::now().timestamp_millis(), "firstTokenAt":first_at,
            "totalDurationMs":duration_ms, "ttftMs":self.first_content_ms, "generationMs":generation_ms,
            "throughputTokensPerSecond":throughput, "inputTokens":input, "outputTokens":output,
            "reasoningTokens":reasoning, "contentTokens":content, "totalTokens":total,
            "cacheReadTokens":cache_read, "cacheWriteTokens":cache_write,
            "finishReason":self.finish_reason, "responseId":self.response_id,
            "responseBlocks":self.blocks, "usage":self.usage, "error":error, "timingSource":"engine_monotonic"});
        self.snapshot
            .as_object_mut()
            .unwrap()
            .extend(fields.as_object().unwrap().clone());
        if status == "completed" && error.is_none() {
            if let Some(reasoning) = self.provider_reasoning.take() {
                self.snapshot["providerReasoning"] = reasoning;
            }
        }
        self.started()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn later_null_usage_does_not_erase_reported_request_or_cache_counters() {
        let mut trace = RequestTrace::new(json!({"id":"request"}));
        trace.observe_openai(&json!({"usage":{"prompt_tokens":11,"completion_tokens":1,
            "prompt_tokens_details":{"cached_tokens":4}}}));
        trace.observe_openai(&json!({"usage":{"prompt_tokens":null,"completion_tokens":2,
            "prompt_tokens_details":{"cached_tokens":null,"cache_write_tokens":3}}}));
        let result = trace.finish("interrupted", None);
        assert_eq!(
            result["usage"],
            json!({"prompt_tokens":11,"completion_tokens":2,
            "prompt_tokens_details":{"cached_tokens":4,"cache_write_tokens":3}})
        );
        assert_eq!(result["inputTokens"], 11);
        assert_eq!(result["outputTokens"], 2);
    }

    #[test]
    fn openai_response_keeps_blocks_usage_only_chunks_and_reasoning_split() {
        let mut trace = RequestTrace::new(json!({"id":"request", "startedAt":1000}));
        trace.observe_openai(
            &json!({"id":"response", "choices":[{"delta":{"reasoning_content":"Think"}}]}),
        );
        trace.observe_openai(&json!({"choices":[{"delta":{"content":"Answer"}}]}));
        trace.observe_openai(&json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call","function":{"name":"Bash","arguments":"{\"command\":"}}]}}]}));
        trace.observe_openai(&json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"pwd\"}"}}]}, "finish_reason":"tool_calls"}]}));
        trace.observe_openai(&json!({"choices":[],"usage":{"prompt_tokens":100,"completion_tokens":305,"total_tokens":405,"completion_tokens_details":{"reasoning_tokens":44}}}));
        let result = trace.finish("completed", None);
        assert_eq!(
            result["responseBlocks"][2]["input"],
            json!({"command":"pwd"})
        );
        assert_eq!(result["reasoningTokens"], 44);
        assert_eq!(result["contentTokens"], 261);
        assert_eq!(result["totalTokens"], 405);
        assert_eq!(result["responseId"], "response");
        assert_eq!(result["finishReason"], "tool_calls");
        assert!(result["firstTokenAt"].is_number());
    }
    #[test]
    fn repeated_empty_tool_name_deltas_keep_the_first_name() {
        let mut trace = RequestTrace::new(json!({"id":"request"}));
        trace.observe_openai(&json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call","type":"function","function":{"name":"Bash","arguments":""}}]}}]}));
        trace.observe_openai(&json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":null,"type":null,"function":{"name":"","arguments":"{\"command\":"}}]}}]}));
        trace.observe_openai(&json!({"choices":[{"delta":{"tool_calls":[{"index":0,"type":null,"function":{"name":"","arguments":"\"pwd\"}"}}]}, "finish_reason":"tool_calls"}]}));
        let result = trace.finish("completed", None);
        assert_eq!(result["responseBlocks"][0]["name"], "Bash");
        assert_eq!(
            result["responseBlocks"][0]["input"],
            json!({"command":"pwd"})
        );
    }

    #[test]
    fn anthropic_cumulative_usage_is_not_double_counted_and_unknown_split_is_null() {
        let mut trace = RequestTrace::new(json!({"id":"request", "startedAt":1000}));
        trace.observe_anthropic(&json!({"type":"message_start", "message":{"usage":{"input_tokens":10,"output_tokens":1,"cache_read_input_tokens":20}}}));
        trace.observe_anthropic(&json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}));
        trace.observe_anthropic(&json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"你好"}}));
        trace.observe_anthropic(&json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":30}}));
        let result = trace.finish("completed", None);
        assert_eq!(result["outputTokens"], 30);
        assert_eq!(result["totalTokens"], 60);
        assert!(result["reasoningTokens"].is_null());
        assert!(result["contentTokens"].is_null());
        assert_eq!(result["responseBlocks"][0]["text"], "你好");
    }
    #[test]
    fn partial_output_alone_does_not_block_recovery_but_every_terminal_signal_does() {
        // Text without a terminal signal: a later body stall is still a stall.
        let mut trace = RequestTrace::new(json!({"id":"request"}));
        trace.observe_openai(&json!({"choices":[{"delta":{"content":"Partial"}}]}));
        assert!(trace.first_content_observed());
        assert!(!trace.post_content_recovery_blocked());

        // A raw `[DONE]` closes the response.
        let mut done = RequestTrace::new(json!({"id":"request"}));
        done.observe_openai(&json!({"choices":[{"delta":{"content":"Partial"}}]}));
        done.mark_completed();
        assert!(done.post_content_recovery_blocked());

        // A finish reason on the leading choice.
        let mut lead = RequestTrace::new(json!({"id":"request"}));
        lead.observe_openai(&json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}));
        assert!(lead.post_content_recovery_blocked());

        // A finish reason on a non-leading choice, which block assembly ignores.
        let mut trailing = RequestTrace::new(json!({"id":"request"}));
        trailing.observe_openai(&json!({"choices":[
            {"delta":{"content":"Partial"},"finish_reason":null},
            {"delta":{},"finish_reason":"tool_calls"}
        ]}));
        assert!(trailing.post_content_recovery_blocked());

        // A provider SSE error event is terminal even if the body then times out.
        let mut remote = RequestTrace::new(json!({"id":"request"}));
        remote.observe_openai(&json!({"error":{"type":"overloaded_error","message":"busy"}}));
        assert!(remote.post_content_recovery_blocked());

        // Direct Chat can forward the top-level form instead; it vetoes too.
        let mut top_level = RequestTrace::new(json!({"id":"request"}));
        top_level.observe_openai(&json!({"type":"error","error":{"type":"overloaded_error"}}));
        assert!(top_level.post_content_recovery_blocked());
        let mut bare = RequestTrace::new(json!({"id":"request"}));
        bare.observe_openai(&json!({"type":"error","message":"busy"}));
        assert!(bare.post_content_recovery_blocked());
    }

    #[test]
    fn recovery_content_counts_every_choice_the_loop_consumes() {
        // A role-only first choice with text on the second: the loop consumes
        // it even though the first-choice assembly sees nothing.
        let mut later_text = RequestTrace::new(json!({"id":"request"}));
        later_text.observe_openai(&json!({"choices":[
            {"delta":{"role":"assistant"},"finish_reason":null},
            {"delta":{"content":"Later"},"finish_reason":null}
        ]}));
        assert!(!later_text.first_content_observed());
        assert!(later_text.openai_recovery_content_observed());
        assert!(!later_text.post_content_recovery_blocked());

        let mut later_thinking = RequestTrace::new(json!({"id":"request"}));
        later_thinking.observe_openai(&json!({"choices":[
            {"delta":{"role":"assistant"},"finish_reason":null},
            {"delta":{"reasoning_content":"Later"},"finish_reason":null}
        ]}));
        assert!(later_thinking.openai_recovery_content_observed());

        let mut later_calls = RequestTrace::new(json!({"id":"request"}));
        later_calls.observe_openai(&json!({"choices":[
            {"delta":{"role":"assistant"},"finish_reason":null},
            {"delta":{"tool_calls":[{"index":0,"id":"call","function":{"name":"Bash"}}]},"finish_reason":null}
        ]}));
        assert!(later_calls.openai_recovery_content_observed());

        // Prelude alone is not content, so A may still retry it.
        let mut prelude = RequestTrace::new(json!({"id":"request"}));
        prelude.observe_openai(&json!({"choices":[
            {"delta":{"role":"assistant"},"finish_reason":null},
            {"delta":{"content":""},"finish_reason":null},
            {"delta":{"tool_calls":[]},"finish_reason":null}
        ]}));
        assert!(!prelude.openai_recovery_content_observed());
        assert!(!prelude.first_content_observed());

        // The adapter's content-start marker still counts as content.
        let mut adapter = RequestTrace::new(json!({"id":"request"}));
        adapter.observe_openai(&json!({"response_content_started":true}));
        assert!(adapter.openai_recovery_content_observed());
    }

    #[test]
    fn no_content_failure_does_not_invent_ttft_or_throughput() {
        let trace = RequestTrace::new(json!({"id":"request", "startedAt":1000}));
        let result = trace.finish("error", Some("Request failed"));
        assert!(result["ttftMs"].is_null());
        assert!(result["throughputTokensPerSecond"].is_null());
        assert_eq!(result["status"], "error");
    }

    #[test]
    fn truncated_response_retains_partial_content_and_is_interrupted() {
        let mut trace = RequestTrace::new(json!({"id":"request", "startedAt":1000}));
        trace.observe_openai(&json!({"choices":[{"delta":{"content":"Partial"}}]}));
        let result = trace.finish("completed", None);
        assert_eq!(result["status"], "stream_error");
        assert_eq!(result["responseBlocks"][0]["text"], "Partial");
        assert!(result["outputTokens"].is_null());
    }

    #[test]
    fn provider_error_event_is_failed_even_when_transport_closes_normally() {
        let mut trace = RequestTrace::new(json!({"id":"request", "startedAt":1000}));
        trace.observe_anthropic(
            &json!({"type":"error", "error":{"type":"overloaded_error", "message":"Overloaded"}}),
        );
        let result = trace.finish("completed", None);
        assert_eq!(result["status"], "error");
        // The provider text is remote data: only the shared vocabulary survives,
        // and the failure is structured so callers can classify it.
        let error = result["error"].as_str().unwrap();
        assert!(!error.contains("Overloaded"), "{error}");
        assert!(error.contains("temporarily unavailable"), "{error}");
        assert_eq!(result["failure"]["stage"], "stream");
        assert_eq!(result["failure"]["class"], "transient");
    }

    #[test]
    fn native_response_preserves_block_order_signatures_and_tool_input() {
        let mut trace = RequestTrace::new(json!({"id":"request", "startedAt":1000}));
        trace.observe_openai(&json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{}"}}]}}]}));
        trace.observe_openai(&json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}], "provider_content":{"content":[
            {"type":"thinking","thinking":"Analysis","thinkingSignature":"signature"},
            {"type":"toolCall","id":"call","name":"Read","arguments":{"path":"file"}},
            {"type":"text","text":"Comment"}
        ]}}));
        let result = trace.finish("completed", None);
        assert_eq!(result["responseBlocks"][0]["text"], "Analysis");
        assert_eq!(
            result["responseBlocks"][0]["thinkingSignature"],
            "signature"
        );
        assert_eq!(result["responseBlocks"][1]["input"], json!({"path":"file"}));
        assert_eq!(result["responseBlocks"][2]["text"], "Comment");
    }

    #[test]
    fn openai_responses_encrypted_reasoning_is_saved_on_completed_snapshot_only() {
        let mut trace = RequestTrace::new(json!({"id": "req-1", "startedAt": 1000}));
        trace.mark_completed();
        trace.set_provider_reasoning(&json!({
            "api": "openai-responses",
            "provider": "java-llm-proxy",
            "model": "gpt-6.2-astra",
            "items": [
                {"type": "reasoning", "id": "rs_sum", "summary": [{"type": "summary_text", "text": "plain"}]},
                {"type": "reasoning", "id": "rs_blank", "encrypted_content": "   "},
                {"type": "reasoning", "id": "rs_enc", "summary": [], "encrypted_content": "opaque-enc"},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "ok"}]}
            ]
        }));
        let completed = trace.finish("completed", None);
        assert_eq!(
            completed["providerReasoning"],
            json!({
                "api": "openai-responses",
                "provider": "java-llm-proxy",
                "model": "gpt-6.2-astra",
                "reasoning_only": true,
                "items": [
                    {"type": "reasoning", "id": "rs_enc", "summary": [], "encrypted_content": "opaque-enc"}
                ]
            })
        );

        let mut errored = RequestTrace::new(json!({"id": "req-2", "startedAt": 1000}));
        errored.mark_completed();
        errored.set_provider_reasoning(&json!({
            "api": "openai-responses",
            "provider": "java-llm-proxy",
            "model": "gpt-6.2-astra",
            "items": [{"type": "reasoning", "id": "rs_enc", "encrypted_content": "opaque-enc"}]
        }));
        let failed = errored.finish("completed", Some("upstream failed"));
        assert!(failed.get("providerReasoning").is_none());

        let mut summary_only = RequestTrace::new(json!({"id": "req-3", "startedAt": 1000}));
        summary_only.mark_completed();
        summary_only.set_provider_reasoning(&json!({
            "api": "openai-responses",
            "provider": "java-llm-proxy",
            "model": "deepseek-v4.1-flash",
            "items": [{"type": "reasoning", "id": "rs_ds", "summary": [{"type": "summary_text", "text": "only summary"}]}]
        }));
        let summary_done = summary_only.finish("completed", None);
        assert!(summary_done.get("providerReasoning").is_none());

        let mut other_api = RequestTrace::new(json!({"id": "req-4", "startedAt": 1000}));
        other_api.mark_completed();
        other_api.set_provider_reasoning(&json!({
            "api": "anthropic-messages",
            "provider": "claude",
            "model": "claude-opus",
            "items": [{"type": "thinking", "thinking": "t", "signature": "sig"}]
        }));
        let other_done = other_api.finish("completed", None);
        assert!(other_done.get("providerReasoning").is_none());
    }
}
