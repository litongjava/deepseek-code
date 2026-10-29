use crate::native_engine::provider_manager::ResolvedProvider;
use crate::tools::ToolDefinition;
use anyhow::Result;
use futures::Stream;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::pin::Pin;
use tokio_stream::StreamExt;

/// Merges adjacent messages that share a role. The Messages API requires
/// alternating roles; Anthropic tolerates and merges repeats server side, but
/// stricter compatible endpoints (e.g. DeepSeek) reject a `tool_use` whose
/// results are split across several user messages.
pub fn merge_consecutive_roles(messages: Vec<AnthropicMessage>) -> Vec<AnthropicMessage> {
    fn into_blocks(content: AnthropicContent) -> Vec<ContentBlock> {
        match content {
            AnthropicContent::Text(text) => vec![ContentBlock::Text { text }],
            AnthropicContent::Blocks(blocks) => blocks,
        }
    }
    let mut merged: Vec<AnthropicMessage> = Vec::with_capacity(messages.len());
    for message in messages {
        match merged.last_mut() {
            Some(last) if last.role == message.role => {
                let previous = std::mem::replace(&mut last.content, AnthropicContent::Blocks(Vec::new()));
                let mut blocks = into_blocks(previous);
                blocks.extend(into_blocks(message.content));
                last.content = AnthropicContent::Blocks(blocks);
            }
            _ => merged.push(message),
        }
    }
    merged
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicMessage {
    pub role: String,
    pub content: AnthropicContent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AnthropicContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image {
        source: ImageSource,
    },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
    #[serde(rename = "thinking")]
    Thinking {
        thinking: String,
        signature: Option<String>,
    },
    #[serde(rename = "redacted_thinking")]
    RedactedThinking {
        data: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageSource {
    #[serde(rename = "type")]
    pub source_type: String,
    pub media_type: String,
    pub data: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AnthropicResponse {
    pub id: String,
    #[serde(rename = "type")]
    pub response_type: String,
    pub role: String,
    pub content: Vec<ContentBlock>,
    pub model: String,
    pub stop_reason: Option<String>,
    pub stop_sequence: Option<String>,
    pub usage: Option<UsageInfo>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UsageInfo {
    pub input_tokens: u32,
    pub output_tokens: u32,
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u32>,
    #[serde(default)]
    pub cache_read_input_tokens: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StreamEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub index: Option<usize>,
    pub delta: Option<StreamDelta>,
    pub content_block: Option<ContentBlock>,
    pub message: Option<StreamMessage>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StreamDelta {
    #[serde(rename = "type")]
    pub delta_type: String,
    pub text: Option<String>,
    pub partial_json: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct StreamMessage {
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub message_type: Option<String>,
    pub role: Option<String>,
    pub content: Option<Vec<ContentBlock>>,
    pub model: Option<String>,
    pub stop_reason: Option<String>,
    pub usage: Option<UsageInfo>,
}


use crate::performance::{retry_with_backoff, checked_response};

pub struct AnthropicClient {
    client: Client,
    response_schema: Option<Value>,
    retry_requests: bool,
}

impl AnthropicClient {
    pub fn new() -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_else(|_| Client::new());
        Self { client, response_schema: None, retry_requests: true }
    }

    /// Isolated classifiers use one attempt and an outer cancellation deadline.
    pub(crate) fn without_retries(mut self) -> Self {
        self.retry_requests = false;
        self
    }

    pub fn with_response_schema(mut self, schema: Option<Value>) -> Self {
        self.response_schema = schema;
        self
    }

    fn apply_options(&self, body: &mut Value, provider: &ResolvedProvider) -> Result<()> {
        crate::services::anthropic_auth::apply_fast(body, provider.model.fast, &provider.model.id, &provider.provider.base_url)?;
        if let Some(schema) = &self.response_schema {
            if crate::structured_output::provider_enforces_schema(&provider.provider.base_url) {
                body["output_config"]["format"] = json!({"type":"json_schema","schema":schema});
            } else {
                crate::structured_output::append_anthropic_instruction(body, schema);
            }
        }
        Ok(())
    }

    pub async fn send_message(
        &self,
        provider: &ResolvedProvider,
        messages: Vec<AnthropicMessage>,
        system_prompt: Option<&str>,
        tools: Vec<ToolDefinition>,
        max_tokens: u32,
    ) -> Result<AnthropicResponse> {
        let base_url = provider.provider.base_url.trim_end_matches('/');
        let url = if base_url.contains("/v1") {
            format!("{}/messages", base_url)
        } else {
            format!("{}/v1/messages", base_url)
        };

        let mut body = json!({
            "model": provider.model.id,
            "max_tokens": max_tokens,
            "messages": merge_consecutive_roles(messages),
        });

        if let Some(system) = system_prompt {
            body["system"] = json!(system);
            if crate::performance::accepts_mid_system(base_url) { crate::performance::move_system_tail(&mut body); }
        }

        if !tools.is_empty() {
            let tool_defs: Vec<Value> = tools.iter().map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                })
            }).collect();
            body["tools"] = json!(tool_defs);
        }

        if let Some(effort) = provider.model.effort.as_deref() { apply_effort(&mut body, effort, &provider.model.id, max_tokens)?; }
        self.apply_options(&mut body, provider)?;
        crate::performance::apply_prompt_cache(&mut body);
        let request = || async {
            let request = self.client
                .post(&url)
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .json(&body);
            let response = crate::services::anthropic_auth::authorize(request, &provider.provider.api_key, base_url, provider.model.fast).await?.send().await?;

            if !self.retry_requests {
                return super::auto_permissions::bounded_response(response).await;
            }
            let response = checked_response(response).await?;

            response.json().await.map_err(|e| anyhow::anyhow!("{:#}", e))
        };
        let data: AnthropicResponse = if self.retry_requests { retry_with_backoff(request).await? } else { request().await? };

        Ok(data)
    }

    pub async fn send_message_stream(
        &self,
        provider: &ResolvedProvider,
        messages: Vec<AnthropicMessage>,
        system_prompt: Option<&str>,
        tools: Vec<ToolDefinition>,
        max_tokens: u32,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<String>> + Send>>> {
        let base_url = provider.provider.base_url.trim_end_matches('/');
        let url = if base_url.contains("/v1") {
            format!("{}/messages", base_url)
        } else {
            format!("{}/v1/messages", base_url)
        };

        let mut body = json!({
            "model": provider.model.id,
            "max_tokens": max_tokens,
            "messages": merge_consecutive_roles(messages),
            "stream": true,
        });

        if let Some(system) = system_prompt {
            body["system"] = json!(system);
            if crate::performance::accepts_mid_system(base_url) { crate::performance::move_system_tail(&mut body); }
        }

        if !tools.is_empty() {
            let tool_defs: Vec<Value> = tools.iter().map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                })
            }).collect();
            body["tools"] = json!(tool_defs);
        }

        if let Some(effort) = provider.model.effort.as_deref() { apply_effort(&mut body, effort, &provider.model.id, max_tokens)?; }
        self.apply_options(&mut body, provider)?;
        crate::performance::apply_prompt_cache(&mut body);
        let response = retry_with_backoff(|| async {
            let request = self.client
                .post(&url)
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .json(&body);
            let resp = crate::services::anthropic_auth::authorize(request, &provider.provider.api_key, base_url, provider.model.fast).await?.send().await?;

            let resp = checked_response(resp).await?;
            Ok(resp)
        }).await?;

        let stream = response.bytes_stream();
        let event_stream = stream
            .map(|chunk| {
                match chunk {
                    Ok(bytes) => {
                        let text = String::from_utf8_lossy(&bytes);
                        Ok(text.to_string())
                    }
                    Err(e) => Err(anyhow::anyhow!("Stream error: {}", e)),
                }
            });

        Ok(Box::pin(event_stream))
    }

    pub async fn send_message_stream_with_thinking(
        &self,
        provider: &ResolvedProvider,
        messages: Vec<AnthropicMessage>,
        system_prompt: Option<&str>,
        tools: Vec<ToolDefinition>,
        max_tokens: u32,
        enable_thinking: bool,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<String>> + Send>>> {
        self.send_message_stream_with_thinking_observed(provider, messages, system_prompt, tools, max_tokens, enable_thinking, None).await
    }

    pub async fn send_message_stream_with_thinking_observed(
        &self,
        provider: &ResolvedProvider,
        messages: Vec<AnthropicMessage>,
        system_prompt: Option<&str>,
        tools: Vec<ToolDefinition>,
        max_tokens: u32,
        enable_thinking: bool,
        context_capture: Option<&super::request_context::ContextCapture>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<String>> + Send>>> {
        let base_url = provider.provider.base_url.trim_end_matches('/');
        let url = if base_url.contains("/v1") {
            format!("{}/messages", base_url)
        } else {
            format!("{}/v1/messages", base_url)
        };

        let mut body = json!({
            "model": provider.model.id,
            "max_tokens": max_tokens,
            "messages": merge_consecutive_roles(messages),
            "stream": true,
        });

        if let Some(system) = system_prompt {
            body["system"] = json!(system);
            if crate::performance::accepts_mid_system(base_url) { crate::performance::move_system_tail(&mut body); }
        }

        if !tools.is_empty() {
            let tool_defs: Vec<Value> = tools.iter().map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                })
            }).collect();
            body["tools"] = json!(tool_defs);
        }

        if let Some(effort) = provider.model.effort.as_deref() {
            apply_effort(&mut body, effort, &provider.model.id, max_tokens)?;
        } else if enable_thinking {
            body["thinking"] = json!({"type":"enabled", "budget_tokens": 10000u32.min(max_tokens.saturating_sub(1))});
        }

        self.apply_options(&mut body, provider)?;
        crate::performance::apply_conversation_cache(&mut body);
        let response = retry_with_backoff(|| async {
            let request = self.client
                .post(&url)
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .json(&body);
            let request = crate::services::anthropic_auth::authorize(request, &provider.provider.api_key, base_url, provider.model.fast).await?;
            if let Some(capture) = context_capture { capture.record("anthropic-messages", &body, provider.model.context_window, max_tokens).await; }
            let resp = request.send().await?;

            let resp = checked_response(resp).await?;
            Ok(resp)
        }).await?;

        let stream = response.bytes_stream();
        let event_stream = stream
            .map(|chunk| {
                match chunk {
                    Ok(bytes) => {
                        let text = String::from_utf8_lossy(&bytes);
                        Ok(text.to_string())
                    }
                    Err(e) => Err(anyhow::anyhow!("Stream error: {}", e)),
                }
            });

        Ok(Box::pin(event_stream))
    }
}

impl Default for AnthropicClient {
    fn default() -> Self {
        Self::new()
    }
}

pub fn apply_effort(body: &mut Value, effort: &str, model: &str, max_tokens: u32) -> Result<()> {
    if effort == "off" { body["thinking"] = json!({"type":"disabled"}); return Ok(()); }
    if model.contains("opus-4-6") || model.contains("opus-4-7") || model.contains("sonnet-4-6") {
        body["thinking"] = json!({"type":"adaptive"});
        body["output_config"] = json!({"effort": if effort == "max" && model.contains("sonnet") { "high" } else { effort }});
    } else {
        if max_tokens <= 1024 { anyhow::bail!("Thinking requires max output tokens greater than 1024"); }
        let budget = match effort { "low" => 2048, "medium" => 5000, "high" => 10000, "xhigh" => 16000, "max" => 32000, _ => anyhow::bail!("Invalid effort") };
        body["thinking"] = json!({"type":"enabled", "budget_tokens": budget.min(max_tokens - 1)});
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(content: AnthropicContent) -> AnthropicMessage {
        AnthropicMessage { role: "user".into(), content }
    }

    fn tool_result(id: &str) -> ContentBlock {
        ContentBlock::ToolResult { tool_use_id: id.into(), content: json!("ok"), is_error: Some(false) }
    }

    #[test]
    fn split_tool_results_and_follow_up_text_merge_into_one_user_message() {
        let messages = vec![
            user(AnthropicContent::Text("task".into())),
            AnthropicMessage {
                role: "assistant".into(),
                content: AnthropicContent::Blocks(vec![
                    ContentBlock::ToolUse { id: "a".into(), name: "Read".into(), input: json!({}) },
                    ContentBlock::ToolUse { id: "b".into(), name: "Bash".into(), input: json!({}) },
                ]),
            },
            user(AnthropicContent::Blocks(vec![tool_result("a")])),
            user(AnthropicContent::Blocks(vec![tool_result("b")])),
            user(AnthropicContent::Text("peer note".into())),
        ];
        let merged = serde_json::to_value(merge_consecutive_roles(messages)).unwrap();
        assert_eq!(merged.as_array().unwrap().len(), 3);
        let roles: Vec<_> = merged.as_array().unwrap().iter().map(|m| m["role"].clone()).collect();
        assert_eq!(roles, vec![json!("user"), json!("assistant"), json!("user")]);
        let last = &merged[2]["content"];
        assert_eq!(last[0]["tool_use_id"], "a");
        assert_eq!(last[1]["tool_use_id"], "b");
        assert_eq!(last[2], json!({"type": "text", "text": "peer note"}));
    }

    #[test]
    fn alternating_messages_are_unchanged() {
        let messages = vec![
            user(AnthropicContent::Text("hi".into())),
            AnthropicMessage { role: "assistant".into(), content: AnthropicContent::Text("hello".into()) },
        ];
        let merged = serde_json::to_value(merge_consecutive_roles(messages)).unwrap();
        assert_eq!(merged, json!([
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "hello"}
        ]));
    }
}
