//! Isolated, single-evaluation prompt hook with a deadline covering transport retries. A successful evaluation grants no permissions.
use super::*;
use crate::native_engine::{
    anthropic_client::{AnthropicClient, AnthropicContent, AnthropicMessage, ContentBlock},
    openai_client::{OpenAIClient, OpenAIContent, OpenAIMessage},
    provider_manager::{ApiFormat, ResolvedProvider},
};

const SYSTEM: &str = "You are evaluating a hook in Claude Code.\n\nYour response must be a JSON object matching one of the following schemas:\n1. If the condition is met, return: {\"ok\": true}\n2. If the condition is not met, return: {\"ok\": false, \"reason\": \"Reason for why it is not met\"}";
const MAX_TOKENS: u32 = 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PromptHook {
    #[serde(rename = "type")]
    kind: String,
    pub(super) prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
}

pub(super) fn load(value: &mut Value, event: HookEvent) -> Result<Option<PromptHook>> {
    if value["type"] != "prompt" && value["type"] != "agent" {
        return Ok(None);
    }
    ensure!(
        matches!(
            event,
            HookEvent::PreToolUse
                | HookEvent::PostToolUse
                | HookEvent::PostToolUseFailure
                | HookEvent::PermissionRequest
                | HookEvent::UserPromptSubmit
                | HookEvent::Stop
                | HookEvent::SubagentStop
        ),
        "Prompt/agent hooks are not supported for {}",
        event.name()
    );
    for key in value.as_object().context("Invalid prompt hook")?.keys() {
        ensure!(
            [
                "type",
                "prompt",
                "model",
                "timeout",
                "if",
                "once",
                "statusMessage"
            ]
            .contains(&key.as_str()),
            "Unsupported prompt/agent hook option: {key}"
        );
    }
    let config: PromptHook = serde_json::from_value(value.clone())?;
    ensure!(
        !config.prompt.trim().is_empty(),
        "Hook prompt cannot be empty"
    );
    value["command"] = json!(config.prompt);
    if value.get("timeout").is_none() {
        value["timeout"] = json!(if config.is_agent() { 60 } else { 30 });
    }
    Ok(Some(config))
}

impl PromptHook {
    pub(super) fn is_agent(&self) -> bool {
        self.kind == "agent"
    }
}

impl HookRunner {
    pub fn with_prompt_provider(mut self, provider: ResolvedProvider) -> Result<Self> {
        for hook in &self.hooks {
            if let Some(config) = &hook.prompt {
                select_provider(config, &provider)?;
            }
        }
        self.prompt_provider = Some(provider);
        Ok(self)
    }
}

pub(super) fn select_provider(
    config: &PromptHook,
    current: &ResolvedProvider,
) -> Result<ResolvedProvider> {
    let mut provider = current.clone();
    if let Some(model) = &config.model {
        provider.model = provider
            .provider
            .models
            .iter()
            .find(|m| m.enabled && &m.id == model)
            .with_context(|| {
                format!("Prompt hook model {model} is not enabled in the session provider")
            })?
            .clone();
    }
    ensure!(
        provider.model.supports_text,
        "Prompt hook model must support text"
    );
    provider.model.effort = None;
    provider.model.fast = false;
    Ok(provider)
}

pub(super) fn arguments(prompt: &str, input: &str) -> String {
    // JSON is a shell-quoted argument string upstream, not an array of JSON fields.
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' && quote != Some('\'') {
            if let Some(next) = chars.peek().copied() {
                if quote.is_none() || matches!(next, '"' | '\\' | '$' | '`') {
                    word.push(chars.next().unwrap());
                    continue;
                }
            }
        }
        if quote == Some(ch) {
            quote = None;
        } else if quote.is_none() && matches!(ch, '\'' | '"') {
            quote = Some(ch);
        } else if quote.is_none() && ch.is_whitespace() {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
        } else {
            word.push(ch);
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    if quote.is_some() {
        words = input.split_whitespace().map(str::to_owned).collect();
    }
    let indexed = regex::Regex::new(r"\$ARGUMENTS\[(\d+)\]").unwrap();
    let replace = |captures: &regex::Captures<'_>| {
        captures[1]
            .parse::<usize>()
            .ok()
            .and_then(|i| words.get(i))
            .cloned()
            .unwrap_or_default()
    };
    let text = indexed.replace_all(prompt, &replace);
    let shorthand = regex::Regex::new(r"\$(\d+)\b").unwrap();
    let text = shorthand
        .replace_all(&text, &replace)
        .replace("$ARGUMENTS", input);
    if text == prompt && !input.is_empty() {
        format!("{text}\n\nARGUMENTS: {input}")
    } else {
        text
    }
}

async fn evaluate(
    config: &PromptHook,
    current: Option<&ResolvedProvider>,
    input: &Value,
) -> Result<Value> {
    let provider = select_provider(
        config,
        current.context("Prompt hook has no session provider")?,
    )?;
    let prompt = arguments(&config.prompt, &serde_json::to_string(input)?);
    let budget = provider
        .model
        .max_tokens
        .unwrap_or(MAX_TOKENS)
        .min(MAX_TOKENS);
    ensure!(budget > 0, "Prompt hook output budget must be positive");
    ensure!(
        prompt.len() + SYSTEM.len() + budget as usize
            <= provider.model.context_window.unwrap_or(32_768) as usize,
        "Prompt hook input exceeds context budget"
    );
    let schema = json!({"type":"object","properties":{"ok":{"type":"boolean"},"reason":{"type":"string"}},"required":["ok"],"additionalProperties":false});
    // Like generation, all built-ins use the native protocol router. Its transport
    // retries remain inside execute_prompt's deadline; there is no evaluation loop.
    let text = match (
        provider.provider.builtin_id.is_some(),
        &provider.provider.api_format,
    ) {
        (true, _) | (_, ApiFormat::OpenAI) => {
            let response = OpenAIClient::new()
                .with_request_options(
                    crate::native_engine::request_options::RequestOptions::for_session(
                        input["session_id"].as_str(),
                    ),
                )
                .without_retries()
                .with_response_schema(Some(schema))
                .send_message(
                    &provider,
                    vec![OpenAIMessage {
                        role: "user".into(),
                        content: OpenAIContent::Text(prompt),
                        tool_calls: None,
                        tool_call_id: None,
                        reasoning_content: None,
                        provider_content: None,
                    }],
                    Some(SYSTEM),
                    vec![],
                    budget,
                )
                .await?;
            let choice = response
                .choices
                .into_iter()
                .next()
                .context("Prompt hook returned no choice")?;
            ensure!(
                choice.finish_reason.as_deref() == Some("stop")
                    && choice.message.tool_calls.as_ref().is_none_or(Vec::is_empty),
                "Prompt hook returned incomplete output or tools"
            );
            match choice.message.content {
                OpenAIContent::Text(text) => text,
                _ => bail!("Prompt hook requires text output"),
            }
        }
        (false, ApiFormat::Anthropic) => {
            let response = AnthropicClient::new()
                .with_request_options(
                    crate::native_engine::request_options::RequestOptions::for_session(
                        input["session_id"].as_str(),
                    ),
                )
                .without_retries()
                .with_response_schema(Some(schema))
                .send_message(
                    &provider,
                    vec![AnthropicMessage {
                        role: "user".into(),
                        content: AnthropicContent::Text(prompt),
                    }],
                    Some(SYSTEM),
                    vec![],
                    budget,
                )
                .await?;
            ensure!(
                matches!(
                    response.stop_reason.as_deref(),
                    Some("end_turn" | "stop_sequence")
                ),
                "Prompt hook returned incomplete output"
            );
            let mut text = String::new();
            for block in response.content {
                match block {
                    ContentBlock::Text { text: part } => text.push_str(&part),
                    _ => bail!("Prompt hook requires text output without tools"),
                }
            }
            text
        }
    };
    let result: Value =
        serde_json::from_str(text.trim()).context("Prompt hook JSON validation failed")?;
    ensure!(
        valid_result(&result),
        "Prompt hook schema validation failed"
    );
    Ok(result)
}

pub(super) fn valid_result(value: &Value) -> bool {
    value["ok"].is_boolean() && value.get("reason").is_none_or(Value::is_string)
}

pub(super) async fn execute_prompt(
    hook: &ActiveHook,
    config: &PromptHook,
    provider: Option<&ResolvedProvider>,
    input: &Value,
) -> Result<(i32, String, String)> {
    let _progress = super::super::hook_progress::Running::start(
        input["session_id"].as_str().unwrap_or(""),
        hook.status_message.as_deref(),
    );
    match tokio::time::timeout(
        Duration::from_secs(hook.timeout),
        evaluate(config, provider, input),
    )
    .await
    {
        Ok(Ok(value)) if value["ok"] == true => Ok((0, String::new(), String::new())),
        Ok(Ok(value)) => Ok((
            2,
            String::new(),
            format!(
                "Prompt hook condition was not met: {}",
                value["reason"].as_str().unwrap_or("undefined")
            ),
        )),
        Ok(Err(error)) => Ok((1, String::new(), format!("Prompt hook failed: {error:#}"))),
        Err(_) => Ok((1, String::new(), "Prompt hook cancelled: timed out".into())),
    }
}

#[cfg(test)]
#[path = "tool_hook_prompt_tests.rs"]
mod tests;
