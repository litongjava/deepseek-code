//! Delegate a task to another coding agent installed on this machine:
//! deepseek-code (`dcl`), Claude Code (`claude`) or OpenAI Codex (`codex`). A
//! tool is offered only when its executable is on PATH; signing in to an agent
//! or configuring its provider is left to the user.
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::time::Duration;

const DEFAULT_TIMEOUT_SECS: u64 = 600;
const MAX_TIMEOUT_SECS: u64 = 3600;
/// Bytes of an agent's answer returned to the model.
const MAX_OUTPUT: usize = 60_000;

struct Agent {
    tool: &'static str,
    command: &'static str,
    name: &'static str,
    login: &'static str,
}

/// Set for a `dcl` started by this tool, so it does not offer the tool again.
const NESTED_ENV: &str = "DCL_EXTERNAL_AGENT";

const AGENTS: &[Agent] = &[
    Agent { tool: "DeepSeekCode", command: "dcl", name: "deepseek-code", login: "run `dcl` once in a terminal and configure a model provider, or pass its API key settings" },
    Agent { tool: "ClaudeCode", command: "claude", name: "Claude Code", login: "run `claude` once in a terminal and sign in, or set ANTHROPIC_API_KEY" },
    Agent { tool: "Codex", command: "codex", name: "OpenAI Codex", login: "run `codex login`, or configure a model provider in ~/.codex/config.toml" },
];

impl Agent {
    /// `dcl` and `claude` share the headless JSON interface and resumable sessions.
    fn resumable(&self) -> bool {
        matches!(self.command, "dcl" | "claude")
    }
}

fn agent(tool: &str) -> Option<&'static Agent> {
    AGENTS.iter().find(|agent| agent.tool == tool)
}

pub fn is_external_agent(tool: &str) -> bool {
    agent(tool).is_some()
}

fn schema(agent: &Agent) -> Value {
    let name = agent.name;
    let mut schema = json!({
        "type": "object",
        "properties": {
            "prompt": {"type": "string", "description": format!("The complete task for {name}. It does not see this conversation, so include every file, constraint and expected result it needs.")},
            "mode": {"type": "string", "enum": ["read-only", "edit"], "default": "read-only", "description": "read-only: inspect and answer only. edit: may modify files in the project."},
            "model": {"type": "string", "description": "Optional model for the agent; its own default otherwise."},
            "timeout_secs": {"type": "integer", "minimum": 10, "maximum": MAX_TIMEOUT_SECS, "default": DEFAULT_TIMEOUT_SECS}
        },
        "required": ["prompt"],
        "additionalProperties": false
    });
    if agent.resumable() {
        schema["properties"]["session_id"] = json!({"type": "string", "description": format!("Continue the {name} session with this session_id from an earlier result, keeping its context. Omit to start a new session.")});
    }
    schema
}

/// Definitions for the agents installed on this machine.
pub(super) fn definitions() -> Vec<super::ToolDefinition> {
    AGENTS
        .iter()
        .filter(|agent| !(agent.command == "dcl" && std::env::var_os(NESTED_ENV).is_some()))
        .filter(|agent| super::lsp_servers::find_on_path(agent.command).is_some())
        .map(|agent| super::ToolDefinition {
            name: agent.tool.into(),
            description: format!(
                "Delegate a self-contained task to {} (the `{}` CLI installed on this machine) and return its final answer. Use it for a second opinion, a review, or work the user explicitly wants {} to do. It runs non-interactively in the project directory: read-only by default, mode=edit lets it change files. It cannot ask questions, so give it the full context.{} Requires approval.",
                agent.name, agent.command, agent.name,
                if agent.resumable() { format!(" Each call starts a new {} session unless session_id continues an earlier one from a previous result.", agent.name) } else { String::new() }
            ),
            input_schema: schema(agent),
        })
        .collect()
}

fn arguments(agent: &Agent, input: &Value, prompt: &str, cwd: &str, answer_file: &std::path::Path) -> Result<Vec<String>> {
    let edit = match input["mode"].as_str().unwrap_or("read-only") {
        "read-only" => false,
        "edit" => true,
        other => bail!("mode must be read-only or edit, not {other}"),
    };
    let mut args: Vec<String> = Vec::new();
    match agent.command {
        "claude" | "dcl" => {
            // A headless agent denies anything that would need approval;
            // edit mode lets it change files but still not run commands.
            args.extend(["-p", "--output-format", "json", "--permission-mode"].map(String::from));
            args.push(if edit { "acceptEdits" } else { "default" }.into());
            if let Some(session) = input["session_id"].as_str().filter(|id| !id.trim().is_empty()) {
                // Session IDs are UUIDs; anything else could be read as an option.
                uuid::Uuid::parse_str(session.trim()).with_context(|| format!("session_id must be a {} session UUID, not {session}", agent.name))?;
                args.extend(["--resume".into(), session.trim().into()]);
            }
        }
        _ => {
            anyhow::ensure!(input["session_id"].is_null(), "{} sessions cannot be continued from here; start a new one", agent.name);
            args.extend(["exec", "--skip-git-repo-check", "--sandbox"].map(String::from));
            args.push(if edit { "workspace-write" } else { "read-only" }.into());
            args.extend(["--cd".into(), cwd.into(), "--output-last-message".into(), answer_file.to_string_lossy().into_owned()]);
        }
    }
    if let Some(model) = input["model"].as_str().filter(|model| !model.trim().is_empty()) {
        args.extend(["--model".into(), model.into()]);
    }
    // The prompt is one argument after `--`, so it is never read as an option.
    args.extend(["--".into(), prompt.into()]);
    Ok(args)
}

fn clip(text: &str) -> String {
    if text.len() <= MAX_OUTPUT {
        return text.to_owned();
    }
    let mut end = MAX_OUTPUT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[output truncated]", &text[..end])
}

fn tail(text: &str, lines: usize) -> String {
    let rows: Vec<&str> = text.lines().filter(|line| !line.trim().is_empty()).collect();
    rows[rows.len().saturating_sub(lines)..].join("\n")
}

pub(super) async fn execute(tool: &str, input: Value, cwd: &str) -> Result<Value> {
    let agent = agent(tool).context("Unknown external agent")?;
    let prompt = input["prompt"].as_str().map(str::trim).filter(|prompt| !prompt.is_empty()).context("prompt is required")?;
    let program = super::lsp_servers::find_on_path(agent.command)
        .with_context(|| format!("{} is not installed: `{}` was not found on PATH", agent.name, agent.command))?;
    let timeout = input["timeout_secs"].as_u64().unwrap_or(DEFAULT_TIMEOUT_SECS).clamp(10, MAX_TIMEOUT_SECS);
    let answer_dir = tempfile::tempdir()?;
    let answer_file = answer_dir.path().join("answer.txt");
    let mut command = tokio::process::Command::new(&program);
    command
        .args(arguments(agent, &input, prompt, cwd, &answer_file)?)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    if agent.command == "dcl" {
        command.env(NESTED_ENV, "1");
    }
    #[cfg(unix)]
    command.process_group(0);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let started = std::time::Instant::now();
    let child = command.spawn().with_context(|| format!("Cannot start {}", program.display()))?;
    // Stops the agent and everything it started when the call is cancelled.
    let _tree = crate::services::tool_hooks::ProcessTree::new(child.id().context("The agent has no process ID")?)?;
    let output = match tokio::time::timeout(Duration::from_secs(timeout), child.wait_with_output()).await {
        Ok(output) => output?,
        Err(_) => bail!("{} did not finish within {timeout} seconds and was stopped", agent.name),
    };
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let mut cost = None;
    let mut session = None;
    let (answer, failed) = if agent.resumable() {
        match serde_json::from_str::<Value>(stdout.trim()) {
            Ok(result) => {
                cost = result["total_cost_usd"].as_f64();
                session = result["session_id"].as_str().map(str::to_owned);
                (
                    result["result"].as_str().unwrap_or_default().to_owned(),
                    result["is_error"] == true || !output.status.success(),
                )
            }
            Err(_) => (stdout.trim().to_owned(), !output.status.success()),
        }
    } else {
        let answer = std::fs::read_to_string(&answer_file).unwrap_or_default();
        (answer.trim().to_owned(), !output.status.success())
    };
    if failed {
        let detail = if answer.is_empty() { tail(&format!("{stdout}\n{stderr}"), 12) } else { answer };
        bail!(
            "{} failed ({}):\n{}\nIf this is a sign-in or API key problem, ask the user to fix it: {}.",
            agent.name,
            output.status,
            clip(&detail),
            agent.login
        );
    }
    Ok(json!({
        "agent": agent.name,
        "mode": input["mode"].as_str().unwrap_or("read-only"),
        "answer": clip(&answer),
        "elapsed_seconds": started.elapsed().as_secs(),
        "cost_usd": cost,
        "session_id": session,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_map_modes_and_keep_the_prompt_after_double_dash() {
        let file = std::path::Path::new("/tmp/answer.txt");
        let claude = arguments(&AGENTS[1], &json!({"mode":"edit","model":"sonnet"}), "--help me", "/p", file).unwrap();
        assert_eq!(claude, ["-p", "--output-format", "json", "--permission-mode", "acceptEdits", "--model", "sonnet", "--", "--help me"]);
        let codex = arguments(&AGENTS[2], &json!({}), "review", "/p", file).unwrap();
        assert_eq!(codex, ["exec", "--skip-git-repo-check", "--sandbox", "read-only", "--cd", "/p", "--output-last-message", "/tmp/answer.txt", "--", "review"]);
        assert!(arguments(&AGENTS[2], &json!({"mode":"anything"}), "x", "/p", file).is_err());
        let id = "0cc3f1f4-67cc-4d6d-aa91-953fcc8eb695";
        for agent in [&AGENTS[0], &AGENTS[1]] {
            let resumed = arguments(agent, &json!({"session_id":id}), "continue", "/p", file).unwrap();
            assert_eq!(resumed, ["-p", "--output-format", "json", "--permission-mode", "default", "--resume", id, "--", "continue"]);
            assert!(arguments(agent, &json!({"session_id":"--dangerously-skip-permissions"}), "x", "/p", file).is_err());
            assert!(schema(agent)["properties"]["session_id"].is_object());
        }
        assert!(arguments(&AGENTS[2], &json!({"session_id":id}), "x", "/p", file).is_err());
        assert!(schema(&AGENTS[2])["properties"]["session_id"].is_null());
    }

    #[tokio::test]
    async fn missing_prompt_and_unknown_agents_are_rejected() {
        assert!(execute("Codex", json!({}), ".").await.unwrap_err().to_string().contains("prompt"));
        assert!(execute("Other", json!({"prompt":"x"}), ".").await.is_err());
    }
}
