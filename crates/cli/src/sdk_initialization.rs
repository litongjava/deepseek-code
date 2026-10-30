//! Per-run SDK initialization. Validate and gather metadata before publishing overrides.
use crate::{args::Cli, session::Session};
#[path = "sdk_agents.rs"]
mod agents;
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};

#[derive(Default)]
pub struct State {
    initialized: bool,
}

#[derive(Default)]
pub struct Overrides {
    pub system_prompt: Option<String>,
    pub response_schema: Option<Value>,
    /// Prepended once to the first user turn when a main-thread agent is selected.
    pub initial_prompt: Option<String>,
    pub prompt_suggestions: bool,
    pub agent_progress_summaries: bool,
    /// Settings applied by `apply_flag_settings`, reported by `get_settings`.
    pub flag_settings: serde_json::Map<String, Value>,
    /// SDK user message UUIDs mapped to persisted message ids, for rewind_files.
    pub message_ids: std::collections::HashMap<String, String>,
}

impl State {
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    pub fn start_user_turn(&mut self) {
        self.initialized = true;
    }

    pub async fn initialize(
        &mut self,
        cli: &Cli,
        session: &mut Session,
        request: &Value,
    ) -> Result<Value> {
        ensure!(!self.initialized, "Already initialized");
        let mut overrides = validate(cli, request)?;
        let servers = request.get("sdkMcpServers").map(deepseek_code_core::mcp::sdk::validate_names).transpose()?.unwrap_or_default();
        session.validate_sdk_mcp_servers(&servers).await?;
        let mcp_host = session.sdk_mcp_transport.clone().context("SDK MCP transport unavailable")?;
        ensure!(session.sdk_hooks.is_some(), "SDK hook transport unavailable");
        let callbacks = request
            .get("hooks")
            .map(deepseek_code_core::services::hook_callbacks::validate)
            .transpose()?
            .unwrap_or_default();
        let catalog = agents::prepare(session, request.get("agents")).await?;
        if let Some(name) = request.get("agent").and_then(Value::as_str) {
            let agent = catalog
                .iter()
                .find(|agent| agent.definition.id == name)
                .with_context(|| format!("initialize.agent is not a defined agent: {name}"))?;
            let mut prompt = agent.definition.system_prompt.clone();
            if let Some(reminder) = &agent.reminder {
                prompt.push_str("\n\nCritical system reminder:\n");
                prompt.push_str(reminder);
            }
            if let Some(existing) = overrides.system_prompt.take() {
                if !existing.is_empty() {
                    prompt.push_str("\n\n");
                    prompt.push_str(&existing);
                }
            }
            overrides.system_prompt = Some(prompt);
            overrides.initial_prompt = agent.initial_prompt.clone();
            if let Some(provider) = &agent.provider {
                session.model = provider.model.id.clone();
                session.provider_id = provider.provider.id.clone();
            }
            if let Some(mode) = &agent.permission_mode {
                session
                    .permissions
                    .set_mode(deepseek_code_core::permissions::PermissionMode::parse(mode)?);
            }
        }
        let mut response = metadata(session).await?;
        let entries = response["agents"]
            .as_array_mut()
            .expect("metadata agents array");
        for agent in catalog.iter() {
            entries.retain(|entry| entry["name"] != agent.definition.id);
            let mut entry =
                json!({"name":agent.definition.id,"description":agent.definition.description});
            if let Some(provider) = &agent.provider {
                entry["model"] = json!(provider.model.id);
            }
            entries.push(entry);
        }
        // Nothing above mutates the running session, including a failed metadata read.
        session.sdk_mcp.install(servers, mcp_host)?;
        session
            .sdk_hooks
            .as_mut()
            .context("SDK hook transport unavailable")?
            .install(&session.id, callbacks);
        session.sdk_overrides = overrides;
        session.engine.set_runtime_agents(&session.id, catalog);
        self.initialized = true;
        Ok(response)
    }
}

fn validate(cli: &Cli, request: &Value) -> Result<Overrides> {
    let fields = request
        .as_object()
        .context("initialize request must be an object")?;
    let mut system = None;
    let mut append = None;
    let mut schema = None;
    let mut suggestions = false;
    let mut summaries = false;
    for (key, value) in fields {
        match key.as_str() {
            "subtype" => ensure!(value == "initialize", "Expected initialize subtype"),
            "systemPrompt" | "appendSystemPrompt" => {
                let text = value
                    .as_str()
                    .with_context(|| format!("initialize.{key} must be a string"))?;
                ensure!(text.len() <= 1_048_576, "initialize.{key} exceeds 1 MiB");
                if key == "systemPrompt" {
                    system = Some(text.to_owned());
                } else {
                    append = Some(text.to_owned());
                }
            }
            "jsonSchema" => {
                deepseek_code_core::structured_output::validate_schema(value)
                    .context("Invalid initialize.jsonSchema")?;
                schema = Some(value.clone());
            }
            "agents" => ensure!(value.is_object(), "initialize.agents must be an object"),
            "hooks" => {
                deepseek_code_core::services::hook_callbacks::validate(value)?;
            }
            "sdkMcpServers" => { deepseek_code_core::mcp::sdk::validate_names(value)?; },
            "promptSuggestions" => {
                suggestions = value
                    .as_bool()
                    .with_context(|| "initialize.promptSuggestions must be a boolean")?;
            }
            "agentProgressSummaries" => {
                summaries = value
                    .as_bool()
                    .with_context(|| "initialize.agentProgressSummaries must be a boolean")?;
            }
            "agent" => ensure!(
                value.as_str().is_some_and(|name| !name.trim().is_empty()),
                "initialize.agent must be a non-empty agent name"
            ),
            _ => bail!("Unknown initialize field: {key}"),
        }
    }
    let system_prompt = if system.is_some() || append.is_some() {
        let mut base = match system {
            Some(value) => value,
            None => match &cli.system_prompt_file {
                Some(path) => {
                    std::fs::read_to_string(path).context("Cannot read UTF-8 system prompt")?
                }
                None => cli.system_prompt.clone().unwrap_or_default(),
            },
        };
        let suffix = match append {
            Some(value) => value,
            None => match &cli.append_system_prompt_file {
                Some(path) => std::fs::read_to_string(path)
                    .context("Cannot read UTF-8 appended system prompt")?,
                None => cli.append_system_prompt.clone().unwrap_or_default(),
            },
        };
        if !base.is_empty() && !suffix.is_empty() {
            base.push_str("\n\n");
        }
        base.push_str(&suffix);
        Some(base)
    } else {
        None
    };
    Ok(Overrides {
        system_prompt,
        response_schema: schema,
        initial_prompt: None,
        prompt_suggestions: suggestions,
        agent_progress_summaries: summaries,
        flag_settings: Default::default(),
        message_ids: Default::default(),
    })
}

async fn metadata(session: &Session) -> Result<Value> {
    let context = crate::services::context(&session.paths)?;
    let skills =
        crate::skill_commands::load_for_workspace(&session.paths, &session.workspace).await?;
    let commands = crate::skill_commands::catalog(&skills).into_iter().map(|(name, description)| {
        json!({"name":name.trim_start_matches('/'),"description":description,"argumentHint":""})
    }).collect::<Vec<_>>();
    let agents = deepseek_code_core::services::agents::list(&context)?
        .into_iter()
        .map(|agent| {
            // Agent model overrides are not applied by the current execution engine.
            json!({"name":agent.id,"description":agent.description})
        })
        .collect::<Vec<_>>();
    let mut styles = std::collections::BTreeSet::from([
        "default".to_owned(),
        "explanatory".to_owned(),
        "learning".to_owned(),
    ]);
    let directory = session.workspace.join(".claude/output-styles");
    if directory.exists() {
        for entry in std::fs::read_dir(&directory)? {
            let path = entry?.path();
            if path.is_file() && path.extension().is_some_and(|ext| ext == "md") {
                if let Some(name) = path.file_stem().and_then(|name| name.to_str()) {
                    if !name.is_empty()
                        && name
                            .chars()
                            .all(|c| c.is_alphanumeric() || c == '-' || c == '_')
                    {
                        styles.insert(name.to_owned());
                    }
                }
            }
        }
    }
    Ok(json!({"commands":commands,"agents":agents,
        "output_style":crate::display_commands::load(session)?.output_style,
        "available_output_styles":styles,"models":session.sdk_models().await,
        "account":{},"pid":std::process::id()}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn overrides_preserve_unspecified_cli_parts_and_reject_unsupported_payloads() {
        let cli = Cli::parse_from([
            "dcl",
            "--system-prompt",
            "CLI_BASE",
            "--append-system-prompt",
            "CLI_APPEND",
        ]);
        assert_eq!(
            validate(&cli, &json!({"systemPrompt":"SDK"}))
                .unwrap()
                .system_prompt
                .as_deref(),
            Some("SDK\n\nCLI_APPEND")
        );
        assert_eq!(
            validate(&cli, &json!({"appendSystemPrompt":"SDK"}))
                .unwrap()
                .system_prompt
                .as_deref(),
            Some("CLI_BASE\n\nSDK")
        );
        assert_eq!(
            validate(&cli, &json!({"systemPrompt":"","appendSystemPrompt":""}))
                .unwrap()
                .system_prompt
                .as_deref(),
            Some("")
        );
        for bad in [
            json!({"systemPrompt":null}),
            json!({"jsonSchema":{"type":42}}),
            json!({"jsonSchema":{"$ref":"https://invalid/schema"}}),
            json!({"unknown":false}),
            json!({"hooks":{"PreToolUse":[{"hookCallbackIds":[""]}]}}),
            json!({"agents":[]}),
            json!({"sdkMcpServers":["host","host"]}),
            json!({"promptSuggestions":"yes"}),
        ] {
            assert!(validate(&cli, &bad).is_err(), "{bad}");
        }
    }
}
