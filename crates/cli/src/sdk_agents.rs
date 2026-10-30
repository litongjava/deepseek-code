//! SDK agent descriptions are validated as one run-local transaction.
use crate::session::Session;
use anyhow::{bail, ensure, Context, Result};
use deepseek_code_core::{
    native_engine::runtime_agents::{Catalog, RuntimeAgent},
    services::agents,
};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Agent {
    description: String,
    prompt: String,
    tools: Option<Vec<String>>,
    #[serde(default)]
    disallowed_tools: Vec<String>,
    model: Option<String>,
    max_turns: Option<u32>,
    #[serde(default)]
    mcp_servers: Vec<Value>,
    #[serde(default)]
    skills: Vec<String>,
    #[serde(default)]
    background: bool,
    memory: Option<String>,
    permission_mode: Option<String>,
    effort: Option<Value>,
    initial_prompt: Option<String>,
    #[serde(rename = "criticalSystemReminder_EXPERIMENTAL")]
    reminder: Option<String>,
}

fn effort(value: &Value) -> Result<String> {
    let text = value
        .as_str()
        .context("SDK agent effort must be a named level; numeric effort is not supported")?;
    ensure!(
        matches!(text, "low" | "medium" | "high" | "xhigh" | "max" | "off"),
        "Invalid SDK agent effort"
    );
    Ok(text.into())
}

fn mcp_servers(value: &[Value]) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in value {
        let Some(name) = entry.as_str() else {
            bail!("SDK agent mcpServers must name configured servers; inline server definitions are not supported");
        };
        ensure!(
            !name.trim().is_empty() && name.len() <= 256,
            "Invalid SDK agent MCP server name"
        );
        ensure!(!names.iter().any(|existing| existing == name), "Duplicate SDK agent MCP server: {name}");
        names.push(name.into());
    }
    Ok(names)
}

async fn skill_context(session: &Session, names: &[String]) -> Result<Option<String>> {
    if names.is_empty() {
        return Ok(None);
    }
    ensure!(names.len() <= 100, "Too many SDK agent skills");
    let manager = deepseek_code_core::skills::SkillsManager::with_path(
        session.paths.data_dir.join("skills"),
    );
    let skills = manager.load_skills().await?;
    let mut context = String::new();
    for name in names {
        let skill = skills
            .iter()
            .find(|skill| {
                skill.enabled
                    && (skill.name.eq_ignore_ascii_case(name)
                        || skill.id.eq_ignore_ascii_case(name))
            })
            .with_context(|| format!("SDK agent skill not found or disabled: {name}"))?;
        let content = skill
            .content
            .as_deref()
            .filter(|content| !content.trim().is_empty())
            .with_context(|| format!("SDK agent skill has no instructions: {name}"))?;
        context.push_str(&format!("# Preloaded skill: {}\n\n{}\n\n", skill.name, content));
    }
    ensure!(
        context.len() <= 256 * 1024,
        "SDK agent skill context exceeds 256 KiB"
    );
    Ok(Some(context))
}

pub async fn prepare(session: &Session, value: Option<&Value>) -> Result<Catalog> {
    let definitions: BTreeMap<String, Agent> = match value {
        None => return Ok(Default::default()),
        Some(value) => {
            serde_json::from_value(value.clone()).context("Invalid initialize.agents")?
        }
    };
    ensure!(definitions.len() <= 100, "Too many SDK agent definitions");
    let mut catalog = Vec::new();
    for (id, agent) in definitions {
        ensure!(
            !agent.background,
            "SDK agent {id}: background agents are not supported"
        );
        ensure!(
            !agent.description.trim().is_empty() && agent.description.len() <= 32_000,
            "Invalid SDK agent description"
        );
        ensure!(
            agent.disallowed_tools.len() <= 100
                && agent
                    .disallowed_tools
                    .iter()
                    .all(|name| !name.trim().is_empty()),
            "Invalid SDK agent disallowedTools"
        );
        ensure!(
            agent.max_turns.is_none_or(|turns| turns > 0),
            "SDK agent maxTurns must be positive"
        );
        if let Some(tools) = &agent.tools {
            ensure!(
                tools.len() <= 256 && tools.iter().all(|name| !name.trim().is_empty()),
                "Invalid SDK agent tools"
            );
        }
        let mcp_servers = mcp_servers(&agent.mcp_servers)?;
        let skill_context = skill_context(session, &agent.skills).await?;
        let memory = match agent.memory.as_deref() {
            None => None,
            Some(scope @ ("user" | "project" | "local")) => Some(scope.to_owned()),
            Some(other) => bail!("Invalid SDK agent memory scope: {other}"),
        };
        let permission_mode = agent
            .permission_mode
            .as_deref()
            .map(|mode| {
                deepseek_code_core::permissions::PermissionMode::parse(mode)
                    .map(|mode| mode.as_str().to_owned())
            })
            .transpose()
            .with_context(|| format!("Invalid SDK agent permissionMode for {id}"))?;
        if let Some(mode) = permission_mode.as_deref() {
            let mode = deepseek_code_core::permissions::PermissionMode::parse(mode)?;
            ensure!(
                mode != deepseek_code_core::permissions::PermissionMode::Auto,
                "SDK agent {id}: auto mode is not supported"
            );
            ensure!(
                mode != deepseek_code_core::permissions::PermissionMode::BypassPermissions
                    || session.permissions.get_mode()
                        == deepseek_code_core::permissions::PermissionMode::BypassPermissions,
                "SDK agent {id}: permissionMode may not exceed the session mode"
            );
        }
        let effort = agent.effort.as_ref().map(effort).transpose()?;
        if let Some(initial) = &agent.initial_prompt {
            ensure!(
                initial.len() <= 1024 * 1024,
                "SDK agent initialPrompt exceeds 1 MiB"
            );
        }
        if let Some(reminder) = &agent.reminder {
            ensure!(
                reminder.len() <= 64 * 1024,
                "SDK agent criticalSystemReminder exceeds 64 KiB"
            );
        }
        let definition = agents::Definition {
            id: id.clone(),
            description: agent.description,
            system_prompt: agent.prompt,
            tools: agent.tools,
            legacy: None,
        };
        agents::validate_custom(&definition)?;
        let provider = match agent.model.as_deref() {
            None | Some("inherit") => None,
            Some(model) => Some(session.sdk_resolve_model(model).await?),
        };
        catalog.push(RuntimeAgent {
            definition,
            provider,
            disallowed_tools: agent.disallowed_tools,
            max_turns: agent.max_turns,
            mcp_servers,
            skill_context,
            memory,
            permission_mode,
            effort,
            initial_prompt: agent.initial_prompt,
            reminder: agent.reminder,
        });
    }
    Ok(std::sync::Arc::new(catalog))
}
