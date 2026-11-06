//! User overrides of the system prompt and tools, shared by Desktop and the CLI.
//! Saved as one JSON file in the data directory and read at the start of each use,
//! so a change applies to the next model request without a restart.
use crate::tools::ToolDefinition;
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    time::Duration,
};

const FILE: &str = "customization.json";
const DEFAULT_TIMEOUT_SECS: u64 = 120;
const MAX_TIMEOUT_SECS: u64 = 3600;
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PromptMode {
    /// The built-in prompt, unchanged.
    #[default]
    Default,
    /// The built-in prompt followed by the user's text.
    Append,
    /// The user's text instead of the built-in prompt.
    Replace,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SystemPrompt {
    pub mode: PromptMode,
    pub text: String,
}

/// A tool the user adds: the model's input arrives as JSON on the command's
/// standard input and in `TOOL_INPUT`, and its standard output is the result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CustomTool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub command: String,
    #[serde(default = "enabled_by_default")]
    pub enabled: bool,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

fn enabled_by_default() -> bool {
    true
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Customization {
    pub system_prompt: SystemPrompt,
    pub disabled_tools: Vec<String>,
    pub tool_descriptions: BTreeMap<String, String>,
    pub custom_tools: Vec<CustomTool>,
}

pub fn path(data_dir: &Path) -> PathBuf {
    data_dir.join(FILE)
}

/// A missing file means no customization. A damaged one is reported, not ignored,
/// so disabled tools can never silently come back.
pub fn load(data_dir: &Path) -> Result<Customization> {
    match std::fs::read(path(data_dir)) {
        Ok(bytes) => serde_json::from_slice(&bytes).with_context(|| format!("Invalid {FILE}")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Customization::default()),
        Err(error) => Err(error.into()),
    }
}

pub fn save(data_dir: &Path, customization: &Customization, builtin: &[String]) -> Result<()> {
    customization.validate(builtin)?;
    std::fs::create_dir_all(data_dir)?;
    let mut file = tempfile::NamedTempFile::new_in(data_dir)?;
    std::io::Write::write_all(&mut file, &serde_json::to_vec_pretty(customization)?)?;
    file.persist(path(data_dir))?;
    Ok(())
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|first| first.is_ascii_alphabetic())
        && name.len() <= 64
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
        && !name.starts_with("mcp__")
}

impl Customization {
    pub fn validate(&self, builtin: &[String]) -> Result<()> {
        ensure!(
            self.system_prompt.mode != PromptMode::Replace || !self.system_prompt.text.trim().is_empty(),
            "The replacement system prompt cannot be empty"
        );
        let mut seen = std::collections::HashSet::new();
        for tool in &self.custom_tools {
            ensure!(valid_name(&tool.name), "Invalid tool name: {}. Use letters, digits, _ and -, starting with a letter", tool.name);
            ensure!(!builtin.contains(&tool.name), "{} is already a built-in tool", tool.name);
            ensure!(seen.insert(tool.name.as_str()), "Duplicate tool name: {}", tool.name);
            ensure!(!tool.description.trim().is_empty(), "{} needs a description", tool.name);
            ensure!(!tool.command.trim().is_empty(), "{} needs a command", tool.name);
            ensure!(tool.input_schema["type"] == "object", "{}: the input schema must be a JSON object schema", tool.name);
            ensure!(tool.timeout_secs.is_none_or(|secs| (1..=MAX_TIMEOUT_SECS).contains(&secs)), "{}: the timeout must be 1 to {MAX_TIMEOUT_SECS} seconds", tool.name);
        }
        Ok(())
    }

    pub fn is_disabled(&self, name: &str) -> bool {
        self.disabled_tools.iter().any(|disabled| disabled == name)
    }

    pub fn custom_tool(&self, name: &str) -> Option<&CustomTool> {
        self.custom_tools.iter().find(|tool| tool.enabled && tool.name == name)
    }

    /// The tools the model sees: built-ins as edited, without disabled ones, then added tools.
    pub fn apply(&self, tools: Vec<ToolDefinition>) -> Vec<ToolDefinition> {
        let mut tools: Vec<ToolDefinition> = tools
            .into_iter()
            .filter(|tool| !self.is_disabled(&tool.name))
            .map(|mut tool| {
                if let Some(description) = self.tool_descriptions.get(&tool.name).filter(|text| !text.trim().is_empty()) {
                    tool.description = description.clone();
                }
                tool
            })
            .collect();
        tools.extend(self.custom_tools.iter().filter(|tool| tool.enabled).map(|tool| ToolDefinition {
            name: tool.name.clone(),
            description: tool.description.clone(),
            input_schema: tool.input_schema.clone(),
        }));
        tools
    }
}

/// Runs an added tool with the session's working directory and the user's rights.
pub async fn run_custom_tool(tool: &CustomTool, input: &Value, cwd: &str) -> Result<Value> {
    use std::process::Stdio;
    use tokio::io::AsyncWriteExt;
    let payload = input.to_string();
    let mut command = if cfg!(windows) {
        let mut command = tokio::process::Command::new("powershell");
        // Read the JSON payload and files as UTF-8, not the ANSI code page.
        command.args(["-NoProfile", "-Command", &crate::tools::powershell_command(&tool.command)]);
        command
    } else {
        let mut command = tokio::process::Command::new("sh");
        command.args(["-c", &tool.command]);
        command
    };
    let mut child = command
        .current_dir(cwd)
        .env("TOOL_INPUT", &payload)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("Cannot start tool {}", tool.name))?;
    if let Some(mut stdin) = child.stdin.take() {
        // A command that ignores its input may close the pipe first.
        let _ = stdin.write_all(payload.as_bytes()).await;
    }
    let seconds = tool.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS);
    let output = tokio::time::timeout(Duration::from_secs(seconds), child.wait_with_output())
        .await
        .map_err(|_| anyhow::anyhow!("Tool {} timed out after {seconds}s", tool.name))??;
    let text = |bytes: &[u8]| String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_OUTPUT_BYTES)]).into_owned();
    ensure!(
        output.status.success(),
        "Tool {} failed with {}: {}",
        tool.name,
        output.status,
        text(&output.stderr).trim()
    );
    Ok(json!({"stdout": text(&output.stdout), "success": true, "exit_code": 0}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(name: &str, command: &str) -> CustomTool {
        CustomTool {
            name: name.into(),
            description: "Test tool".into(),
            input_schema: json!({"type":"object","properties":{"text":{"type":"string"}}}),
            command: command.into(),
            enabled: true,
            timeout_secs: None,
        }
    }

    fn definition(name: &str) -> ToolDefinition {
        ToolDefinition { name: name.into(), description: "original".into(), input_schema: json!({"type":"object"}) }
    }

    #[test]
    fn apply_disables_edits_and_adds_tools() {
        let mut customization = Customization::default();
        customization.disabled_tools.push("Write".into());
        customization.tool_descriptions.insert("Read".into(), "edited".into());
        customization.tool_descriptions.insert("Grep".into(), "  ".into());
        customization.custom_tools.push(tool("shout", "cat"));
        let mut off = tool("quiet", "cat");
        off.enabled = false;
        customization.custom_tools.push(off);
        let tools = customization.apply(vec![definition("Read"), definition("Write"), definition("Grep")]);
        let summary: Vec<_> = tools.iter().map(|tool| (tool.name.as_str(), tool.description.as_str())).collect();
        assert_eq!(summary, [("Read", "edited"), ("Grep", "original"), ("shout", "Test tool")]);
        assert!(customization.custom_tool("shout").is_some());
        assert!(customization.custom_tool("quiet").is_none());
    }

    #[test]
    fn invalid_settings_are_rejected_and_never_saved() {
        let dir = tempfile::tempdir().unwrap();
        let builtin = vec!["Read".to_owned()];
        let with = |tool: CustomTool| Customization { custom_tools: vec![tool], ..Default::default() };
        let mut no_schema = tool("ok", "cat");
        no_schema.input_schema = json!({"type":"string"});
        let mut long = tool("ok", "cat");
        long.timeout_secs = Some(MAX_TIMEOUT_SECS + 1);
        for (bad, message) in [
            (with(tool("1bad", "cat")), "Invalid tool name"),
            (with(tool("bad name", "cat")), "Invalid tool name"),
            (with(tool("mcp__x", "cat")), "Invalid tool name"),
            (with(tool("Read", "cat")), "already a built-in"),
            (with(tool("ok", " ")), "needs a command"),
            (with(no_schema), "JSON object schema"),
            (with(long), "timeout"),
            (Customization { custom_tools: vec![tool("a", "x"), tool("a", "y")], ..Default::default() }, "Duplicate"),
            (Customization { system_prompt: SystemPrompt { mode: PromptMode::Replace, text: " ".into() }, ..Default::default() }, "cannot be empty"),
        ] {
            let error = save(dir.path(), &bad, &builtin).unwrap_err().to_string();
            assert!(error.contains(message), "{error}");
        }
        assert!(!path(dir.path()).exists());
    }

    #[test]
    fn settings_survive_a_save_and_damage_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()).unwrap(), Customization::default());
        let saved = Customization {
            system_prompt: SystemPrompt { mode: PromptMode::Append, text: "Be brief".into() },
            disabled_tools: vec!["Write".into()],
            custom_tools: vec![tool("shout", "cat")],
            ..Default::default()
        };
        save(dir.path(), &saved, &[]).unwrap();
        assert_eq!(load(dir.path()).unwrap(), saved);
        std::fs::write(path(dir.path()), "{broken").unwrap();
        assert!(load(dir.path()).is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn custom_tools_receive_input_and_report_failures() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        let input = json!({"text":"hello"});
        let stdin = run_custom_tool(&tool("a", "cat"), &input, cwd).await.unwrap();
        assert_eq!(stdin["stdout"], input.to_string());
        let env = run_custom_tool(&tool("a", "printf %s \"$TOOL_INPUT\"; pwd >&2"), &input, cwd).await.unwrap();
        assert_eq!(env["stdout"], input.to_string());
        let failed = run_custom_tool(&tool("a", "echo nope >&2; exit 3"), &input, cwd).await.unwrap_err().to_string();
        assert!(failed.contains("nope"), "{failed}");
        let mut slow = tool("a", "sleep 30");
        slow.timeout_secs = Some(1);
        assert!(run_custom_tool(&slow, &input, cwd).await.unwrap_err().to_string().contains("timed out"));
    }
}
