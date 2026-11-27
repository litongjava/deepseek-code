use crate::native_engine::provider_manager::ResolvedProvider;
use crate::tools::ToolDefinition;
use anyhow::{Context, Result};
use futures::Stream;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::pin::Pin;
use tokio_stream::StreamExt;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAIMessage {
    pub role: String,
    pub content: OpenAIContent,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<OpenAIToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_content: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OpenAIContent {
    Text(String),
    Multi(Vec<OpenAIContentPart>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum OpenAIContentPart {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image_url")]
    Image { image_url: ImageUrl },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageUrl {
    pub url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenAIToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OpenAIResponse {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<OpenAIChoice>,
    pub usage: Option<OpenAIUsage>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OpenAIChoice {
    pub index: usize,
    pub message: OpenAIMessage,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OpenAIUsage {
    #[serde(flatten)]
    pub extra: std::collections::BTreeMap<String, Value>,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    /// Providers that report reasoning accounting put it here; omission or
    /// null means unknown, never zero.
    #[serde(default)]
    pub completion_tokens_details: Option<CompletionTokensDetails>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CompletionTokensDetails {
    #[serde(default)]
    pub reasoning_tokens: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OpenAIStreamChunk {
    pub id: String,
    pub object: String,
    pub created: u64,
    pub model: String,
    pub choices: Vec<OpenAIStreamChoice>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OpenAIStreamChoice {
    pub index: usize,
    pub delta: OpenAIDelta,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OpenAIDelta {
    pub role: Option<String>,
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<OpenAIToolCallDelta>>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OpenAIToolCallDelta {
    pub index: usize,
    pub id: Option<String>,
    #[serde(rename = "type")]
    pub call_type: Option<String>,
    pub function: Option<FunctionCallDelta>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FunctionCallDelta {
    pub name: Option<String>,
    pub arguments: Option<String>,
}


use crate::performance::{retry_with_backoff, checked_response};

pub struct OpenAIClient {
    client: Client,
    /// Test injection also covers the streaming transport.
    stream_override: Option<Client>,
    response_schema: Option<Value>,
    retry_requests: bool,
    /// The permission classifier uses one bounded attempt; other callers that
    /// opt out of request-level retries keep the transport's own stream retry.
    classifier_bound: bool,
    pub(super) request_options: super::request_options::RequestOptions,
}

impl OpenAIClient {
    pub fn new() -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .unwrap_or_else(|_| Client::new());
        Self {
            client,
            stream_override: None,
            request_options: Default::default(),
            response_schema: None,
            retry_requests: true,
            classifier_bound: false,
        }
    }

    /// The streaming transport keeps no total deadline; each read is bounded by
    /// the configured idle timeout instead, and shares the process-wide pool.
    pub(super) fn streaming_client(&self) -> Result<Client> {
        if let Some(client) = &self.stream_override {
            return Ok(client.clone());
        }
        super::model_http::streaming_client(super::model_http::Redirect::Default)
    }

    /// A replacement POST must not become two physical requests through a
    /// redirect or reqwest's internal protocol-nack retry.
    pub(super) fn single_post_client(&self) -> Result<Client> {
        if let Some(client) = &self.stream_override {
            return Ok(client.clone());
        }
        super::model_http::single_post_client()
    }

    pub fn with_request_options(mut self, options: super::request_options::RequestOptions) -> Self {
        self.request_options = options;
        self
    }

    #[cfg(test)]
    pub(super) fn with_http_client(mut self, client: Client) -> Self {
        self.client = client.clone();
        self.stream_override = Some(client);
        self
    }

    /// Isolated classifiers use one attempt and an outer cancellation deadline.
    pub(crate) fn without_retries(mut self) -> Self {
        self.retry_requests = false;
        self
    }

    /// The classifier response is small and must not spend a second POST or
    /// grow unbounded. Other no-retry callers keep their normal transport.
    pub(crate) fn with_classifier_bound(mut self) -> Self {
        self.classifier_bound = true;
        self
    }

    pub fn with_response_schema(mut self, schema: Option<Value>) -> Self {
        self.response_schema = schema;
        self
    }

    pub async fn send_message(
        &self,
        provider: &ResolvedProvider,
        messages: Vec<OpenAIMessage>,
        system_prompt: Option<&str>,
        tools: Vec<ToolDefinition>,
        max_tokens: u32,
    ) -> Result<OpenAIResponse> {
        let provider = &provider.current()?;
        anyhow::ensure!(!crate::services::anthropic_auth::is_oauth_marker(&provider.provider.api_key), "Anthropic OAuth credentials require Anthropic messages API");
        if provider.provider.builtin_id.is_some() {
            let mut stream = if !self.classifier_bound {
                super::provider_runtime::stream_with_options(
                    provider,
                    messages,
                    system_prompt,
                    tools,
                    max_tokens,
                    self.response_schema.as_ref(),
                    None,
                    &self.request_options,
                )
                .await?
            } else {
                super::provider_runtime::stream_with_options_counted_budget(
                    provider,
                    messages,
                    system_prompt,
                    tools,
                    max_tokens,
                    self.response_schema.as_ref(),
                    None,
                    &self.request_options,
                    crate::performance::AttemptBudget::single_attempt(),
                )
                .await?
                .0
            };
            let mut text = String::new();
            let mut thinking = String::new();
            let mut calls = std::collections::BTreeMap::<usize, Value>::new();
            let mut native = None;
            let mut usage = None;
            let mut finish = None;
            while let Some(chunk) = stream.next().await {
                // The stream retry gate can release several complete normalized frames together.
                let frames = crate::streaming::sse_parser::consume_sse_payloads(&chunk?);
                anyhow::ensure!(
                    frames.remainder.trim().is_empty(),
                    "Incomplete normalized provider SSE frame"
                );
                for data in frames.payloads {
                    if data == "[DONE]" {
                        continue;
                    }
                    let value: Value = serde_json::from_str(&data)
                        .context("Invalid normalized provider SSE JSON")?;
                    let choice = &value["choices"][0];
                    if let Some(part) = choice["delta"]["content"].as_str() {
                        text.push_str(part);
                    }
                    if let Some(part) = choice["delta"]["reasoning_content"].as_str() {
                        thinking.push_str(part);
                    }
                    if let Some(items) = choice["delta"]["tool_calls"].as_array() {
                        for item in items {
                            let index = item["index"].as_u64().unwrap_or_default() as usize;
                            let call = calls.entry(index).or_insert_with(|| json!({"id":"","type":"function","function":{"name":"","arguments":""}}));
                            if let Some(id) = item["id"].as_str() {
                                call["id"] = json!(id);
                            }
                            for field in ["name", "arguments"] {
                                if let Some(delta) = item["function"][field].as_str() {
                                    call["function"][field] = json!(format!(
                                        "{}{}",
                                        call["function"][field].as_str().unwrap_or_default(),
                                        delta
                                    ));
                                }
                            }
                        }
                    }
                    if let Some(reason) = choice["finish_reason"].as_str() {
                        finish = Some(reason.to_string());
                    }
                    if value.get("provider_content").is_some() {
                        native = Some(value["provider_content"].clone());
                    }
                    // Streaming providers may omit usage or send null until final accounting.
                    // Keep the last reported usage when a later chunk has no accounting.
                    if let Some(reported) = value.get("usage").filter(|usage| !usage.is_null()) {
                        usage = Some(serde_json::from_value(reported.clone())?);
                    }
                }
                // The classifier has no tool loop and expects a tiny JSON answer.
                // Keep the builtin transport under the same bound as the direct
                // without_retries path instead of accepting an unbounded stream.
                if self.classifier_bound
                    && text.len().saturating_add(thinking.len()) > 16 * 1024
                {
                    anyhow::bail!("Classifier response exceeds size limit");
                }
            }
            // Record the account and model that issued this provider-native block so a later
            // turn can tell whether its opaque state is still replayable.
            if let Some(object) = native.as_mut().and_then(Value::as_object_mut) {
                object.insert("provider".into(), json!(provider.provider.id));
                object.insert("model".into(), json!(provider.model.id));
            }
            return Ok(OpenAIResponse { id: uuid::Uuid::new_v4().to_string(), object: "chat.completion".into(), created: 0, model: provider.model.id.clone(),
                choices: vec![OpenAIChoice { index: 0, finish_reason: finish, message: OpenAIMessage { role: "assistant".into(), content: OpenAIContent::Text(text),
                    tool_calls: if calls.is_empty() { None } else { Some(calls.into_values().map(serde_json::from_value).collect::<std::result::Result<Vec<_>,_>>()?) }, tool_call_id: None,
                    reasoning_content: if thinking.is_empty() { None } else { Some(thinking) }, provider_content: native } }], usage });
        }
        let base_url = crate::native_engine::provider_manager::ProviderManager::normalize_base_url(&provider.provider.base_url);
        let url = format!("{}/v1/chat/completions", base_url.trim_end_matches('/'));

        let mut body_messages = Vec::new();
        
        if let Some(system) = system_prompt {
            body_messages.push(OpenAIMessage {
                role: "system".to_string(),
                content: OpenAIContent::Text(system.to_string()),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None, provider_content: None,
            });
        }
        
        body_messages.extend(messages);

        let mut body = json!({
            "model": provider.model.id,
            "max_tokens": max_tokens,
            "messages": body_messages,
        });
        super::tool_call_ids::normalize_chat(&mut body);
        if crate::performance::accepts_mid_system(&base_url) { crate::performance::move_system_tail_chat(&mut body); }

        if !tools.is_empty() {
            let tool_defs: Vec<Value> = tools.iter().map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    }
                })
            }).collect();
            body["tools"] = json!(tool_defs);
        }

        apply_effort(&mut body, provider.model.effort.as_deref(), &provider.provider.base_url);
        if let Some(schema) = &self.response_schema {
            if crate::structured_output::provider_enforces_schema(&provider.provider.base_url) {
                body["response_format"] = json!({"type":"json_schema","json_schema":{"name":"structured_response","strict":true,"schema":schema}});
            } else {
                crate::structured_output::append_openai_instruction(&mut body, schema);
            }
        }
        if let Some(messages) = body["messages"].as_array_mut() {
            for message in messages {
                if let Some(object) = message.as_object_mut() {
                    object.remove("provider_content");
                }
            }
        }
        let harness_headers = self.request_options.headers(provider)?;
        let identity = super::request_identity::RequestIdentity::from_options(
            None,
            provider,
            &self.request_options,
        );
        let request = || async {
            let request = self
                .client
                .post(&url)
                .headers(identity.headers())
                .headers(harness_headers.clone())
                .header(
                    "Authorization",
                    format!("Bearer {}", provider.provider.api_key),
                )
                .header("content-type", "application/json")
                .json(&body);
            let response = super::request_recording::send(
                request,
                &self.request_options,
                provider,
                "openai-completions",
                false,
            )
            .await?;

            if !self.retry_requests {
                return super::auto_permissions::bounded_response(response).await;
            }
            let response = checked_response(response).await?;

            // Keep the reqwest error typed so the retry layer can classify it.
            response
                .json()
                .await
                .map_err(|error| anyhow::Error::new(error).context("Model response was not valid JSON"))
        };
        let data: OpenAIResponse = if self.retry_requests { retry_with_backoff(request).await? } else { request().await? };

        Ok(data)
    }

    pub async fn send_message_stream(
        &self,
        provider: &ResolvedProvider,
        messages: Vec<OpenAIMessage>,
        system_prompt: Option<&str>,
        tools: Vec<ToolDefinition>,
        max_tokens: u32,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<String>> + Send>>> {
        self.send_message_stream_observed(provider, messages, system_prompt, tools, max_tokens, None).await
    }

    pub async fn send_message_stream_observed(
        &self,
        provider: &ResolvedProvider,
        messages: Vec<OpenAIMessage>,
        system_prompt: Option<&str>,
        tools: Vec<ToolDefinition>,
        max_tokens: u32,
        context_capture: Option<&super::request_context::ContextCapture>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<String>> + Send>>> {
        Ok(self
            .send_message_stream_counted(
                provider,
                messages,
                system_prompt,
                tools,
                max_tokens,
                context_capture,
            )
            .await?
            .0)
    }

    /// Same request, but also returns how many HTTP attempts produced it, so a
    /// stream that ends without a completion marker can still report the count.
    pub async fn send_message_stream_counted(
        &self,
        provider: &ResolvedProvider,
        messages: Vec<OpenAIMessage>,
        system_prompt: Option<&str>,
        tools: Vec<ToolDefinition>,
        max_tokens: u32,
        context_capture: Option<&super::request_context::ContextCapture>,
    ) -> Result<(
        Pin<Box<dyn Stream<Item = Result<String>> + Send>>,
        crate::performance::AttemptCounter,
    )> {
        // The budget getter caches built-in defaults after a TOML parse error,
        // so the fallible load must run first. Otherwise the later streaming
        // client validation would accept that cached config and a standalone
        // caller could still put a POST on the wire.
        crate::config::OrchestrationConfig::init_global()?;
        self.send_message_stream_counted_budget(
            provider,
            messages,
            system_prompt,
            tools,
            max_tokens,
            context_capture,
            crate::performance::AttemptBudget::configured(),
        )
        .await
    }

    /// Internal entry that lets the caller own the attempt budget. A
    /// single-post budget is one authorized replacement: it uses a client
    /// without redirects or internal retries.
    pub(crate) async fn send_message_stream_counted_budget(
        &self,
        provider: &ResolvedProvider,
        messages: Vec<OpenAIMessage>,
        system_prompt: Option<&str>,
        tools: Vec<ToolDefinition>,
        max_tokens: u32,
        context_capture: Option<&super::request_context::ContextCapture>,
        budget: crate::performance::AttemptBudget,
    ) -> Result<(
        Pin<Box<dyn Stream<Item = Result<String>> + Send>>,
        crate::performance::AttemptCounter,
    )> {
        let provider = &provider.current()?;
        let initiator = if messages.last().is_some_and(|m| m.role != "user") {
            "agent"
        } else {
            "user"
        };
        let wire = self.request_body(
            provider,
            messages,
            system_prompt,
            tools,
            max_tokens,
            self.request_options.session_id.as_deref(),
        )?;
        if let Some(vision) = wire.builtin_vision {
            let client = if budget.is_single_post() {
                super::model_http::single_post_client()?
            } else {
                super::model_http::streaming_client(super::model_http::Redirect::None)?
            };
            super::provider_runtime::stream_body(
                provider,
                wire.body,
                initiator,
                vision,
                max_tokens,
                context_capture,
                &self.request_options,
                client,
                budget,
            )
            .await
        } else {
            self.stream_body(provider, wire.body, max_tokens, context_capture, budget)
                .await
        }
    }

    pub(super) fn request_body(
        &self,
        provider: &ResolvedProvider,
        messages: Vec<OpenAIMessage>,
        system_prompt: Option<&str>,
        tools: Vec<ToolDefinition>,
        max_tokens: u32,
        session: Option<&str>,
    ) -> Result<super::followup::WireRequest> {
        anyhow::ensure!(!crate::services::anthropic_auth::is_oauth_marker(&provider.provider.api_key), "Anthropic OAuth credentials require Anthropic messages API");
        if provider.provider.builtin_id.is_some() {
            return super::provider_runtime::request_body(
                provider,
                messages,
                system_prompt,
                tools,
                max_tokens,
                self.response_schema.as_ref(),
                session,
            );
        }
        let base_url = crate::native_engine::provider_manager::ProviderManager::normalize_base_url(&provider.provider.base_url);
        let mut body_messages = Vec::new();
        
        if let Some(system) = system_prompt {
            body_messages.push(OpenAIMessage {
                role: "system".to_string(),
                content: OpenAIContent::Text(system.to_string()),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None, provider_content: None,
            });
        }
        
        body_messages.extend(messages);

        let mut body = json!({
            "model": provider.model.id,
            "max_tokens": max_tokens,
            "messages": body_messages,
            "stream": true,
            "stream_options": { "include_usage": true },
        });
        super::tool_call_ids::normalize_chat(&mut body);
        if crate::performance::accepts_mid_system(&base_url) { crate::performance::move_system_tail_chat(&mut body); }

        if !tools.is_empty() {
            let tool_defs: Vec<Value> = tools.iter().map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    }
                })
            }).collect();
            body["tools"] = json!(tool_defs);
        }

        apply_effort(&mut body, provider.model.effort.as_deref(), &provider.provider.base_url);
        if let Some(schema) = &self.response_schema {
            if crate::structured_output::provider_enforces_schema(&provider.provider.base_url) {
                body["response_format"] = json!({"type":"json_schema","json_schema":{"name":"structured_response","strict":true,"schema":schema}});
            } else {
                crate::structured_output::append_openai_instruction(&mut body, schema);
            }
        }
        if let Some(messages) = body["messages"].as_array_mut() { for message in messages { if let Some(object) = message.as_object_mut() { object.remove("provider_content"); } } }
        Ok(super::followup::WireRequest { api: "openai-completions".into(), body, builtin_vision: None })
    }

    pub(super) async fn stream_body(
        &self,
        provider: &ResolvedProvider,
        body: Value,
        max_tokens: u32,
        context_capture: Option<&super::request_context::ContextCapture>,
        budget: crate::performance::AttemptBudget,
    ) -> Result<(
        super::provider_runtime::ProviderStream,
        crate::performance::AttemptCounter,
    )> {
        let base_url = crate::native_engine::provider_manager::ProviderManager::normalize_base_url(
            &provider.provider.base_url,
        );
        let url = format!("{}/v1/chat/completions", base_url.trim_end_matches('/'));

        if let Some(capture) = context_capture {
            capture
                .record_transport(
                    "openai-completions",
                    &body,
                    provider.model.context_window,
                    max_tokens,
                    None,
                    &url,
                    &provider.model.id,
                )
                .await;
        }
        let harness_headers = self.request_options.headers(provider)?;
        let identity = super::request_identity::RequestIdentity::from_options(context_capture, provider, &self.request_options);
        let client = if budget.is_single_post() {
            self.single_post_client()?
        } else {
            self.streaming_client()?
        };
        let request = client.post(&url)
            .headers(harness_headers)
            .header("Authorization", format!("Bearer {}", provider.provider.api_key))
            .header("content-type", "application/json")
            .json(&body);
        super::stream_retry::response_with_budget(
            request,
            identity,
            self.request_options.clone().observed(context_capture),
            provider.clone(),
            "openai-completions",
            budget,
        )
        .await
    }
}

impl Default for OpenAIClient {
    fn default() -> Self {
        Self::new()
    }
}

pub fn apply_effort(body: &mut Value, effort: Option<&str>, base_url: &str) {
    if let Some(effort) = effort {
        if base_url.contains("deepseek.com") {
            body["thinking"] = json!({"type": if effort == "off" { "disabled" } else { "enabled" }});
            if effort != "off" { body["reasoning_effort"] = json!(effort); }
        } else { body["reasoning_effort"] = json!(if effort == "off" { "none" } else { effort }); }
    }
}
