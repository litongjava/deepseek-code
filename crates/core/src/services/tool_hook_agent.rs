//! Bounded verifier. No engine, permission ledger, subprocesses or parent history.
use super::*;
use crate::native_engine::{
    anthropic_client::{AnthropicClient, AnthropicContent, AnthropicMessage, ContentBlock},
    openai_client::{OpenAIClient, OpenAIContent, OpenAIMessage},
    provider_manager::{ApiFormat, ResolvedProvider},
};
use crate::tools::ToolDefinition;
use std::sync::{LazyLock, Mutex};
use tokio::io::AsyncReadExt;

const MAX_TURNS: usize = 50;
const MAX_TOKENS: u32 = 1024;
const SYSTEM: &str = "Verify the hook condition using Read to inspect workspace files. Tools run in an isolated read-only permission context. Other tools are unavailable. Be efficient. Finish by calling StructuredOutput with {\"ok\":true} or {\"ok\":false,\"reason\":\"explanation\"}.";
type Registrations = Vec<(String, uuid::Uuid, tokio::sync::watch::Sender<bool>)>;
static RUNNING: LazyLock<Mutex<Registrations>> = LazyLock::new(|| Mutex::new(Vec::new()));

pub(crate) fn cancel_session(session: &str) {
    for (owner, _, sender) in RUNNING.lock().unwrap().iter() {
        if owner == session {
            let _ = sender.send(true);
        }
    }
}

struct Registration(uuid::Uuid);
impl Drop for Registration {
    fn drop(&mut self) {
        RUNNING.lock().unwrap().retain(|(_, id, _)| *id != self.0);
    }
}

/// This deliberately has no reference to the session's permissions or tool executor.
struct ReadOnlyContext {
    root: PathBuf,
}
impl ReadOnlyContext {
    async fn call(&self, name: &str, input: &Value) -> Result<String> {
        ensure!(
            name == "Read",
            "Agent hook permission denied: {name} is unavailable"
        );
        let object = input.as_object().context("Read requires an object")?;
        ensure!(
            object
                .keys()
                .all(|k| ["file_path", "offset", "limit"].contains(&k.as_str())),
            "Unsupported Read option"
        );
        let path = input["file_path"]
            .as_str()
            .context("Read requires file_path")?;
        let path = tokio::fs::canonicalize(self.root.join(path)).await?;
        ensure!(
            path.starts_with(&self.root),
            "Agent hook permission denied: path outside workspace"
        );
        let file = tokio::fs::File::open(&path).await?;
        ensure!(
            file.metadata().await?.is_file(),
            "Read requires a regular file"
        );
        let mut bytes = Vec::new();
        file.take(65_537).read_to_end(&mut bytes).await?;
        ensure!(
            bytes.len() <= 65_536,
            "Agent hook Read file exceeds 64 KiB limit"
        );
        let text = String::from_utf8(bytes).context("Read requires UTF-8 text")?;
        let number = |key: &str, default| -> Result<usize> {
            match input.get(key) {
                None => Ok(default),
                Some(v) => Ok(v
                    .as_u64()
                    .filter(|n| *n > 0 && *n <= 65_536)
                    .context("Invalid Read offset/limit")? as usize),
            }
        };
        Ok(text
            .lines()
            .skip(number("offset", 1)? - 1)
            .take(number("limit", 2000)?)
            .collect::<Vec<_>>()
            .join("\n"))
    }
}

fn definitions() -> Vec<ToolDefinition> {
    vec![
        ToolDefinition { name: "Read".into(), description: "Read a UTF-8 workspace file (maximum 64 KiB); no external paths or other tools are permitted.".into(), input_schema: json!({"type":"object","properties":{"file_path":{"type":"string"},"offset":{"type":"integer","minimum":1},"limit":{"type":"integer","minimum":1}},"required":["file_path"],"additionalProperties":false}) },
        ToolDefinition { name: "StructuredOutput".into(), description: "Return the final verification result.".into(), input_schema: json!({"type":"object","properties":{"ok":{"type":"boolean"},"reason":{"type":"string"}},"required":["ok"],"additionalProperties":false}) },
    ]
}

fn message(role: &str, text: String, id: Option<String>) -> OpenAIMessage {
    OpenAIMessage {
        role: role.into(),
        content: OpenAIContent::Text(text),
        tool_calls: None,
        tool_call_id: id,
        reasoning_content: None,
        provider_content: None,
    }
}

async fn evaluate(
    config: &prompt::PromptHook,
    current: Option<&ResolvedProvider>,
    input: &Value,
    workspace: &Path,
) -> Result<Option<Value>> {
    let provider = prompt::select_provider(
        config,
        current.context("Agent hook has no session provider")?,
    )?;
    let context = ReadOnlyContext {
        root: tokio::fs::canonicalize(workspace).await?,
    };
    let text = prompt::arguments(&config.prompt, &serde_json::to_string(input)?);
    let mut messages = vec![message("user", text.clone(), None)];
    let mut anthropic = vec![AnthropicMessage {
        role: "user".into(),
        content: AnthropicContent::Text(text),
    }];
    let budget = provider
        .model
        .max_tokens
        .unwrap_or(MAX_TOKENS)
        .min(MAX_TOKENS);
    ensure!(budget > 0, "Agent hook token budget must be positive");
    // Each request is capped, hence the total generation budget is <= 50 * 1024.
    for turn in 0..MAX_TURNS {
        let native = provider.provider.builtin_id.is_some()
            || matches!(provider.provider.api_format, ApiFormat::OpenAI);
        let size = if native {
            serde_json::to_vec(&messages)?.len()
        } else {
            serde_json::to_vec(&anthropic)?.len()
        };
        ensure!(
            size + SYSTEM.len() + budget as usize
                <= provider.model.context_window.unwrap_or(32_768) as usize,
            "Agent hook context budget exceeded"
        );
        let mut calls = Vec::new();
        if native {
            let response = OpenAIClient::new()
                .with_request_options(
                    crate::native_engine::request_options::RequestOptions::for_session(
                        input["session_id"].as_str(),
                    ),
                )
                .without_retries()
                .send_message(
                    &provider,
                    messages.clone(),
                    Some(SYSTEM),
                    definitions(),
                    budget,
                )
                .await?;
            let choice = response
                .choices
                .into_iter()
                .next()
                .context("Agent hook returned no choice")?;
            ensure!(
                matches!(choice.finish_reason.as_deref(), Some("stop" | "tool_calls")),
                "Agent hook incomplete output"
            );
            for call in choice.message.tool_calls.as_deref().unwrap_or_default() {
                calls.push((
                    call.id.clone(),
                    call.function.name.clone(),
                    serde_json::from_str::<Value>(&call.function.arguments)?,
                ));
            }
            messages.push(choice.message);
        } else {
            let response = AnthropicClient::new()
                .with_request_options(
                    crate::native_engine::request_options::RequestOptions::for_session(
                        input["session_id"].as_str(),
                    ),
                )
                .without_retries()
                .send_message(
                    &provider,
                    anthropic.clone(),
                    Some(SYSTEM),
                    definitions(),
                    budget,
                )
                .await?;
            ensure!(
                matches!(
                    response.stop_reason.as_deref(),
                    Some("tool_use" | "end_turn" | "stop_sequence")
                ),
                "Agent hook incomplete output"
            );
            for block in &response.content {
                if let ContentBlock::ToolUse { id, name, input } = block {
                    calls.push((id.clone(), name.clone(), input.clone()));
                }
            }
            anthropic.push(AnthropicMessage {
                role: "assistant".into(),
                content: AnthropicContent::Blocks(response.content),
            });
        }
        // Upstream counts assistant turns before processing structured attachments.
        if turn + 1 == MAX_TURNS {
            return Ok(None);
        }
        if calls.is_empty() {
            let reminder = "You MUST call StructuredOutput to complete this request.";
            messages.push(message("user", reminder.into(), None));
            anthropic.push(AnthropicMessage {
                role: "user".into(),
                content: AnthropicContent::Text(reminder.into()),
            });
            continue;
        }
        ensure!(calls.len() <= 16, "Agent hook tool call budget exceeded");
        let mut results = Vec::new();
        for (id, name, input) in calls {
            if name == "StructuredOutput" {
                // Same schema as prompt hooks and upstream Zod: extra fields are ignored.
                if prompt::valid_result(&input) {
                    return Ok(Some(input));
                }
                return Ok(None);
            }
            let (text, failed) = match context.call(&name, &input).await {
                Ok(text) => (text, false),
                Err(error) => (format!("{error:#}"), true),
            };
            messages.push(message("tool", text.clone(), Some(id.clone())));
            results.push(ContentBlock::ToolResult {
                tool_use_id: id,
                content: json!(text),
                is_error: Some(failed),
            });
        }
        anthropic.push(AnthropicMessage {
            role: "user".into(),
            content: AnthropicContent::Blocks(results),
        });
    }
    Ok(None)
}

pub(super) async fn execute_agent(
    hook: &ActiveHook,
    config: &prompt::PromptHook,
    provider: Option<&ResolvedProvider>,
    input: &Value,
    workspace: &Path,
) -> Result<(i32, String, String)> {
    let session = input["session_id"].as_str().unwrap_or("");
    let _progress =
        super::super::hook_progress::Running::start(session, hook.status_message.as_deref());
    let (sender, mut receiver) = tokio::sync::watch::channel(false);
    let registration = Registration(uuid::Uuid::new_v4());
    RUNNING
        .lock()
        .unwrap()
        .push((session.into(), registration.0, sender));
    let result = tokio::select! {
        biased;
        _ = receiver.changed() => return Ok((3, String::new(), String::new())),
        result = tokio::time::timeout(Duration::from_secs(hook.timeout), evaluate(config, provider, input, workspace)) => result,
    };
    match result {
        Ok(Ok(Some(value))) if value["ok"] == true => Ok((0, String::new(), String::new())),
        Ok(Ok(Some(value))) => Ok((
            2,
            String::new(),
            format!(
                "Agent hook condition was not met: {}",
                value["reason"].as_str().unwrap_or("undefined")
            ),
        )),
        Ok(Ok(None)) | Err(_) => Ok((3, String::new(), String::new())),
        Ok(Err(error)) => Ok((1, String::new(), format!("Agent hook failed: {error:#}"))),
    }
}

#[cfg(test)]
#[path = "tool_hook_agent_tests.rs"]
mod tests;
