//! User overrides of the system prompt and tools, shared by Desktop and the CLI.
//! Saved as one JSON file in the data directory and read at the start of each use,
//! so a change applies to the next model request without a restart.
use crate::tools::ToolDefinition;
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
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

/// Tools that load by default. Every other built-in is available on demand, so
/// the model asks ToolSearch for it before the request that uses it.
pub const CORE_TOOLS: &[&str] = &[
    "Bash",
    "Read",
    "Write",
    "Edit",
    "Grep",
    "Glob",
    "Skill",
    "ToolSearch",
    "AskUserQuestion",
    "EnterPlanMode",
    "ExitPlanMode",
    "plan_mode",
    "TaskOutput",
    "TaskStop",
];

/// Whether the name belongs to the default core set. The list only decides the
/// defaults; both switches stay editable for every tool.
pub fn is_core_tool(name: &str) -> bool {
    CORE_TOOLS.contains(&name)
}

/// The two independent switches of one built-in tool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolLoading {
    /// Load by default: every request carries the definition.
    pub enabled: bool,
    /// The model may load the tool through ToolSearch.
    pub on_demand: bool,
}

/// The switches used when the user has no override for this built-in tool.
pub fn default_loading(name: &str) -> ToolLoading {
    ToolLoading {
        enabled: is_core_tool(name),
        on_demand: true,
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Customization {
    pub system_prompt: SystemPrompt,
    pub disabled_tools: Vec<String>,
    pub tool_descriptions: BTreeMap<String, String>,
    pub custom_tools: Vec<CustomTool>,
    /// Explicit switches per built-in tool. A missing entry follows the default.
    pub tool_loading: BTreeMap<String, ToolLoading>,
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

fn write(data_dir: &Path, customization: &Customization, builtin: &[String]) -> Result<()> {
    customization.validate(builtin)?;
    std::fs::create_dir_all(data_dir)?;
    let mut file = tempfile::NamedTempFile::new_in(data_dir)?;
    std::io::Write::write_all(&mut file, &serde_json::to_vec_pretty(customization)?)?;
    file.persist(path(data_dir))?;
    Ok(())
}

/// A whole-file write, used by tests and by hosts that hold every field.
/// It normalizes the legacy tool entries.
pub fn save(data_dir: &Path, customization: &Customization, builtin: &[String]) -> Result<()> {
    write(data_dir, &customization.migrate(builtin), builtin)
}

/// Saves only the prompt half. The tool half stays byte-for-byte as it was, so
/// saving a prompt never migrates a legacy tool entry or writes a draft.
pub fn save_prompt(
    data_dir: &Path,
    prompt: &SystemPrompt,
    builtin: &[String],
) -> Result<Customization> {
    let mut next = load(data_dir)?;
    next.system_prompt = prompt.clone();
    write(data_dir, &next, builtin)?;
    Ok(next)
}

/// Saves only the tool half. A built-in switch the payload does not mention
/// keeps its effective value, so a legacy entry is folded into the switches
/// instead of being lost; legacy names are then dropped from `disabled_tools`.
pub fn save_tools(
    data_dir: &Path,
    submitted: &Customization,
    builtin: &[String],
) -> Result<Customization> {
    let mut next = load(data_dir)?;
    for name in builtin {
        if let Some(explicit) = submitted.tool_loading.get(name) {
            next.tool_loading.insert(name.clone(), *explicit);
        }
    }
    next.tool_descriptions = submitted.tool_descriptions.clone();
    next.custom_tools = submitted.custom_tools.clone();
    // Both lists are folded, so a legacy name neither the page nor the file
    // mentions is never turned back on by accident. Names outside the catalog
    // keep their old meaning and are carried through unchanged.
    next.disabled_tools
        .extend(submitted.disabled_tools.iter().cloned());
    next.disabled_tools.sort();
    next.disabled_tools.dedup();
    let next = next.migrate(builtin);
    write(data_dir, &next, builtin)?;
    Ok(next)
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic())
        && name.len() <= 64
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
        && !name.starts_with("mcp__")
}

impl Customization {
    pub fn validate(&self, builtin: &[String]) -> Result<()> {
        ensure!(
            self.system_prompt.mode != PromptMode::Replace
                || !self.system_prompt.text.trim().is_empty(),
            "The replacement system prompt cannot be empty"
        );
        let mut seen = std::collections::HashSet::new();
        for tool in &self.custom_tools {
            ensure!(
                valid_name(&tool.name),
                "Invalid tool name: {}. Use letters, digits, _ and -, starting with a letter",
                tool.name
            );
            ensure!(
                !builtin.contains(&tool.name),
                "{} is already a built-in tool",
                tool.name
            );
            ensure!(
                seen.insert(tool.name.as_str()),
                "Duplicate tool name: {}",
                tool.name
            );
            ensure!(
                !tool.description.trim().is_empty(),
                "{} needs a description",
                tool.name
            );
            ensure!(
                !tool.command.trim().is_empty(),
                "{} needs a command",
                tool.name
            );
            ensure!(
                tool.input_schema["type"] == "object",
                "{}: the input schema must be a JSON object schema",
                tool.name
            );
            ensure!(
                tool.timeout_secs
                    .is_none_or(|secs| (1..=MAX_TIMEOUT_SECS).contains(&secs)),
                "{}: the timeout must be 1 to {MAX_TIMEOUT_SECS} seconds",
                tool.name
            );
        }
        Ok(())
    }

    /// Legacy entries. A tool disabled before the switches existed keeps its
    /// "off by default" intent but is still discoverable through ToolSearch.
    pub fn is_disabled(&self, name: &str) -> bool {
        self.disabled_tools.iter().any(|disabled| disabled == name)
    }

    /// The switches that apply to a built-in tool, in this order: the explicit
    /// entry, the legacy list, then the core/non-core default.
    pub fn loading(&self, name: &str) -> ToolLoading {
        if let Some(explicit) = self.tool_loading.get(name) {
            return *explicit;
        }
        if self.is_disabled(name) {
            return ToolLoading {
                enabled: false,
                on_demand: true,
            };
        }
        default_loading(name)
    }

    /// Whether the next request carries this tool. A tool that is only on
    /// demand appears once ToolSearch loaded it in this session.
    pub fn exposes(&self, name: &str, loaded: &BTreeSet<String>) -> bool {
        let loading = self.loading(name);
        loading.enabled || (loading.on_demand && loaded.contains(name))
    }

    pub fn custom_tool(&self, name: &str) -> Option<&CustomTool> {
        self.custom_tools
            .iter()
            .find(|tool| tool.enabled && tool.name == name)
    }

    /// Writes each built-in's effective switches explicitly and drops their
    /// legacy names, so one file no longer mixes the two representations.
    /// Unknown or non-built-in names are left exactly as they were.
    pub fn migrate(&self, builtin: &[String]) -> Customization {
        let mut next = self.clone();
        for name in builtin {
            let explicit = self.tool_loading.contains_key(name);
            if explicit || self.is_disabled(name) || self.loading(name) != default_loading(name) {
                next.tool_loading.insert(name.clone(), self.loading(name));
            }
        }
        next.disabled_tools.retain(|name| !builtin.contains(name));
        next
    }

    /// The definitions with the user's description overrides applied, in their
    /// original order. The caller decides which ones are visible.
    pub fn described(&self, tools: Vec<ToolDefinition>) -> Vec<ToolDefinition> {
        tools
            .into_iter()
            .map(|mut tool| {
                if let Some(description) = self
                    .tool_descriptions
                    .get(&tool.name)
                    .filter(|text| !text.trim().is_empty())
                {
                    tool.description = description.clone();
                }
                tool
            })
            .collect()
    }

    /// The advertised tools, then the user's own enabled ones.
    pub fn apply(&self, tools: Vec<ToolDefinition>) -> Vec<ToolDefinition> {
        let mut tools = self.described(tools);
        tools.extend(
            self.custom_tools
                .iter()
                .filter(|tool| tool.enabled)
                .map(|tool| ToolDefinition {
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                    input_schema: tool.input_schema.clone(),
                }),
        );
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
        command.args([
            "-NoProfile",
            "-Command",
            &crate::tools::powershell_command(&tool.command),
        ]);
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
    let text = |bytes: &[u8]| {
        String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_OUTPUT_BYTES)]).into_owned()
    };
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
        ToolDefinition {
            name: name.into(),
            description: "original".into(),
            input_schema: json!({"type":"object"}),
        }
    }

    #[test]
    fn apply_edits_descriptions_and_adds_tools() {
        let mut customization = Customization::default();
        customization
            .tool_descriptions
            .insert("Read".into(), "edited".into());
        customization
            .tool_descriptions
            .insert("Grep".into(), "  ".into());
        customization.custom_tools.push(tool("shout", "cat"));
        let mut off = tool("quiet", "cat");
        off.enabled = false;
        customization.custom_tools.push(off);
        let tools = customization.apply(vec![definition("Read"), definition("Grep")]);
        let summary: Vec<_> = tools
            .iter()
            .map(|tool| (tool.name.as_str(), tool.description.as_str()))
            .collect();
        assert_eq!(
            summary,
            [
                ("Read", "edited"),
                ("Grep", "original"),
                ("shout", "Test tool")
            ]
        );
        assert!(customization.custom_tool("shout").is_some());
        assert!(customization.custom_tool("quiet").is_none());
    }

    #[test]
    fn only_core_tools_load_by_default_and_every_tool_stays_discoverable() {
        assert_eq!(
            CORE_TOOLS,
            [
                "Bash",
                "Read",
                "Write",
                "Edit",
                "Grep",
                "Glob",
                "Skill",
                "ToolSearch",
                "AskUserQuestion",
                "EnterPlanMode",
                "ExitPlanMode",
                "plan_mode",
                "TaskOutput",
                "TaskStop",
            ]
        );
        for name in CORE_TOOLS {
            assert_eq!(
                default_loading(name),
                ToolLoading {
                    enabled: true,
                    on_demand: true
                },
                "{name}"
            );
        }
        for name in [
            "ListDir",
            "MultiEdit",
            "WebFetch",
            "Agent",
            "Workflow",
            "memory_search",
            "computer_use",
            "ProposeGoal",
        ] {
            assert!(!is_core_tool(name), "{name}");
            assert_eq!(
                default_loading(name),
                ToolLoading {
                    enabled: false,
                    on_demand: true
                },
                "{name}"
            );
        }
        // A tool added later keeps the non-core default.
        assert!(!is_core_tool("SomeFutureTool"));
    }

    #[test]
    fn the_two_switches_are_independent() {
        let mut customization = Customization::default();
        let none = BTreeSet::new();
        assert!(customization.exposes("Grep", &none));
        assert!(!customization.exposes("WebFetch", &none));
        // Legacy "disabled" no longer blocks discovery.
        customization.disabled_tools.push("Grep".into());
        assert_eq!(
            customization.loading("Grep"),
            ToolLoading {
                enabled: false,
                on_demand: true
            }
        );
        assert!(!customization.exposes("Grep", &none));
        let loaded: BTreeSet<String> = ["Grep".to_owned()].into_iter().collect();
        assert!(customization.exposes("Grep", &loaded));
        // Both off means unavailable even after a load.
        customization.tool_loading.insert(
            "Grep".into(),
            ToolLoading {
                enabled: false,
                on_demand: false,
            },
        );
        assert!(!customization.exposes("Grep", &loaded));
        // Turning discovery off leaves a loaded tool in place.
        customization.tool_loading.insert(
            "WebFetch".into(),
            ToolLoading {
                enabled: true,
                on_demand: false,
            },
        );
        assert_eq!(
            customization.loading("WebFetch"),
            ToolLoading {
                enabled: true,
                on_demand: false
            }
        );
        assert!(customization.exposes("WebFetch", &none));
    }

    #[test]
    fn damaged_switch_entries_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        for body in [
            r#"{"tool_loading":{"Grep":{"enabled":false}}}"#,
            r#"{"tool_loading":{"Grep":{"enabled":"no","on_demand":true}}}"#,
            r#"{"tool_loading":{"Grep":true}}"#,
        ] {
            std::fs::write(path(dir.path()), body).unwrap();
            assert!(load(dir.path()).is_err(), "{body}");
        }
    }

    #[test]
    fn saving_migrates_legacy_entries_once_and_keeps_unknown_ones() {
        let dir = tempfile::tempdir().unwrap();
        let builtin = vec!["Grep".to_owned(), "WebFetch".to_owned()];
        let legacy = Customization {
            disabled_tools: vec!["Grep".into(), "WebFetch".into(), "mcp__old__tool".into()],
            ..Default::default()
        };
        save(dir.path(), &legacy, &builtin).unwrap();
        let saved = load(dir.path()).unwrap();
        assert_eq!(
            saved.tool_loading["Grep"],
            ToolLoading {
                enabled: false,
                on_demand: true
            }
        );
        assert_eq!(saved.disabled_tools, ["mcp__old__tool"]);
        // A second save changes nothing.
        save(dir.path(), &saved, &builtin).unwrap();
        assert_eq!(load(dir.path()).unwrap(), saved);
    }

    #[test]
    fn saving_only_prompts_never_migrates_the_tool_half() {
        let dir = tempfile::tempdir().unwrap();
        let builtin = vec!["Write".to_owned(), "Glob".to_owned()];
        let legacy = json!({"disabled_tools":["Write","Glob","mcp__old__tool"],
            "tool_descriptions":{"Glob":"search with the old name"}});
        std::fs::write(path(dir.path()), legacy.to_string()).unwrap();

        let prompt = SystemPrompt {
            mode: PromptMode::Append,
            text: "Be brief".into(),
        };
        let saved = save_prompt(dir.path(), &prompt, &builtin).unwrap();
        assert_eq!(saved.system_prompt, prompt);
        assert!(
            saved.tool_loading.is_empty(),
            "a prompt save must not migrate"
        );
        assert_eq!(saved.disabled_tools, ["Write", "Glob", "mcp__old__tool"]);
        assert_eq!(load(dir.path()).unwrap(), saved);
    }

    #[test]
    fn saving_only_tools_folds_untouched_legacy_entries_and_keeps_unknown_ones() {
        let dir = tempfile::tempdir().unwrap();
        let builtin = vec!["Write".to_owned(), "Glob".to_owned()];
        let legacy = json!({"disabled_tools":["Write","Glob","mcp__old__tool"],
            "tool_descriptions":{"Glob":"search with the old name"}});
        std::fs::write(path(dir.path()), legacy.to_string()).unwrap();

        // The page only touched Glob and kept the old description.
        let submitted = Customization {
            tool_descriptions: BTreeMap::from([(
                "Glob".to_owned(),
                "search with the old name".to_owned(),
            )]),
            disabled_tools: vec!["mcp__old__tool".into()],
            tool_loading: BTreeMap::from([(
                "Glob".to_owned(),
                ToolLoading {
                    enabled: true,
                    on_demand: true,
                },
            )]),
            ..Default::default()
        };
        let saved = save_tools(dir.path(), &submitted, &builtin).unwrap();
        // Write was never mentioned, so its legacy value survives as a switch.
        assert_eq!(
            saved.tool_loading["Write"],
            ToolLoading {
                enabled: false,
                on_demand: true
            }
        );
        assert_eq!(
            saved.tool_loading["Glob"],
            ToolLoading {
                enabled: true,
                on_demand: true
            }
        );
        assert_eq!(saved.disabled_tools, ["mcp__old__tool"]);
        assert_eq!(saved.tool_descriptions["Glob"], "search with the old name");
        assert_eq!(load(dir.path()).unwrap(), saved);

        // Saving the same tool half again does not drift.
        let again = save_tools(dir.path(), &saved, &builtin).unwrap();
        assert_eq!(again, saved);

        // A later prompt save leaves these switches alone.
        let prompt = SystemPrompt {
            mode: PromptMode::Replace,
            text: "Only this".into(),
        };
        let after = save_prompt(dir.path(), &prompt, &builtin).unwrap();
        assert_eq!(after.tool_loading, saved.tool_loading);
        assert_eq!(after.disabled_tools, saved.disabled_tools);
    }

    #[test]
    fn invalid_settings_are_rejected_and_never_saved() {
        let dir = tempfile::tempdir().unwrap();
        let builtin = vec!["Read".to_owned()];
        let with = |tool: CustomTool| Customization {
            custom_tools: vec![tool],
            ..Default::default()
        };
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
            (
                Customization {
                    custom_tools: vec![tool("a", "x"), tool("a", "y")],
                    ..Default::default()
                },
                "Duplicate",
            ),
            (
                Customization {
                    system_prompt: SystemPrompt {
                        mode: PromptMode::Replace,
                        text: " ".into(),
                    },
                    ..Default::default()
                },
                "cannot be empty",
            ),
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
            system_prompt: SystemPrompt {
                mode: PromptMode::Append,
                text: "Be brief".into(),
            },
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
        let stdin = run_custom_tool(&tool("a", "cat"), &input, cwd)
            .await
            .unwrap();
        assert_eq!(stdin["stdout"], input.to_string());
        let env = run_custom_tool(
            &tool("a", "printf %s \"$TOOL_INPUT\"; pwd >&2"),
            &input,
            cwd,
        )
        .await
        .unwrap();
        assert_eq!(env["stdout"], input.to_string());
        let failed = run_custom_tool(&tool("a", "echo nope >&2; exit 3"), &input, cwd)
            .await
            .unwrap_err()
            .to_string();
        assert!(failed.contains("nope"), "{failed}");
        let mut slow = tool("a", "sleep 30");
        slow.timeout_secs = Some(1);
        assert!(run_custom_tool(&slow, &input, cwd)
            .await
            .unwrap_err()
            .to_string()
            .contains("timed out"));
    }
}
