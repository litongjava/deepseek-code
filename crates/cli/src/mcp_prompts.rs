//! MCP prompts as slash commands, as in the reference CLI:
//! `/mcp__SERVER__PROMPT [ARGS...]` runs prompts/get and sends the result.
use crate::session::Session;
use anyhow::{Context, Result};

/// Positional words fill the prompt's arguments in order; the last argument
/// takes the rest of the line.
fn arguments(prompt: &claude_core::mcp::McpPrompt, line: &str) -> Result<serde_json::Value> {
    let mut values = serde_json::Map::new();
    let mut rest = line.trim();
    for (index, argument) in prompt.arguments.iter().enumerate() {
        if rest.is_empty() { break; }
        let value = if index + 1 == prompt.arguments.len() { std::mem::take(&mut rest) } else {
            let (word, tail) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
            rest = tail.trim_start();
            word
        };
        values.insert(argument.name.clone(), value.into());
    }
    let missing: Vec<&str> = prompt.arguments.iter().filter(|a| a.required && !values.contains_key(&a.name)).map(|a| a.name.as_str()).collect();
    anyhow::ensure!(missing.is_empty(), "Missing required argument(s): {}", missing.join(", "));
    Ok(values.into())
}

/// The text to send for an MCP prompt command, or None for other commands.
pub async fn expand(session: &Session, command: &str, line: &str) -> Result<Option<String>> {
    let Some(rest) = command.strip_prefix("/mcp__") else { return Ok(None) };
    let (server, name) = rest.split_once("__").context("Usage: /mcp__SERVER__PROMPT [ARGS...]")?;
    session.ensure_mcp().await?;
    let prompts = session.mcp.get_all_prompts().await;
    let Some((_, prompt)) = prompts.iter().find(|(id, prompt)| id == server && prompt.name == name) else {
        let available: Vec<String> = prompts.iter().map(|(id, prompt)| format!("/mcp__{id}__{}", prompt.name)).collect();
        anyhow::bail!("No MCP prompt {command}. Available: {}", if available.is_empty() { "none".into() } else { available.join(", ") });
    };
    let values = arguments(prompt, line)?;
    session.mcp.get_prompt(server, name, values).await.map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positional_words_fill_arguments_and_the_last_takes_the_rest() {
        let prompt: claude_core::mcp::McpPrompt = serde_json::from_value(serde_json::json!({"name":"review","arguments":[{"name":"pr","required":true},{"name":"focus"}]})).unwrap();
        assert_eq!(arguments(&prompt, "42 error handling paths").unwrap(), serde_json::json!({"pr":"42","focus":"error handling paths"}));
        assert_eq!(arguments(&prompt, "7").unwrap(), serde_json::json!({"pr":"7"}));
        assert!(arguments(&prompt, "").unwrap_err().to_string().contains("pr"));
    }
}
