pub(crate) mod screenshot_file;
mod sync_runtime;
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::RwLock;

pub(crate) mod base64_collapse;
mod lsp;
mod notebook;
pub mod plan_import;
pub(crate) mod read;
mod search;
pub mod tasks;
pub mod toolscript;

pub const TOOL_LOAD_NAME: &str = "ToolLoad";
const MAX_TOOL_LOAD_NAMES: usize = 64;

/// Whether a plugin language server declaration is usable by the LSP tool.
pub fn lsp_config_is_valid(server: &serde_json::Value) -> bool {
    lsp::validate_configuration(server).is_ok()
}
pub mod external_agents;
pub mod lsp_servers;
pub(crate) use lsp::passive_feedback as lsp_passive_feedback;
pub(crate) use lsp::validate_configuration as validate_lsp_configuration;
pub use lsp::{
    configure as configure_lsp_servers, shutdown_all as shutdown_lsp_servers, status as lsp_status,
};
pub mod agent_messages;
mod content_compat;
pub mod coordination;
pub(crate) mod context_search;
pub mod cron_jobs;
mod findings;
pub mod linux_env;
pub mod sandbox;
pub mod scheduled;
pub mod session_worktree;
pub mod teams;
pub(crate) mod utf8;
pub mod wakeup;
mod worktrees;
pub use utf8::powershell_command;
pub(crate) mod apply_patch;
pub(crate) mod apply_patch_apply;
pub(crate) mod apply_patch_fs;
pub(crate) mod apply_patch_tool;
pub mod background_shell;
pub(crate) mod bash_process;
#[cfg(windows)]
mod conpty;
pub mod file_change;
pub(crate) mod github;
pub(crate) mod image_generation;
pub mod monitor;
mod monitor_completion;
pub(crate) mod output_page;
pub(crate) mod path_scope;
pub(crate) mod provider_search;
pub mod python;
mod pty_bash;
#[cfg(target_os = "linux")]
pub(crate) mod pty_gate;
#[cfg(target_os = "linux")]
pub(crate) mod pty_owner;
#[cfg(target_os = "linux")]
pub(crate) mod pty_supervisor;
#[cfg(target_os = "linux")]
pub(crate) mod pty_transport;
mod shell_discovery;
pub(crate) mod shell_input;
mod shell_tool_description;
mod web;
pub use provider_search::search as search_provider;

/// Host entry for the internal managed-PTY roles.
///
/// The supervisor and the execution gate are separate execs of the same host
/// executable. Both must run before the CLI or Desktop initialises a runtime,
/// parses arguments, or opens the database: the supervisor outlives the owner
/// and the gate becomes the user program. Returns `Some(exit_code)` when this
/// process was started as one of those roles.
#[cfg(target_os = "linux")]
#[doc(hidden)]
pub fn run_internal_pty_role() -> anyhow::Result<Option<i32>> {
    if let Some(code) = pty_gate::run_from_env()? {
        return Ok(Some(code));
    }
    if pty_supervisor::run_from_env()? {
        return Ok(Some(0));
    }
    Ok(None)
}

#[cfg(windows)]
#[doc(hidden)]
pub fn run_internal_pty_role() -> anyhow::Result<Option<i32>> {
    conpty::run_gate()
}

#[cfg(not(any(target_os = "linux", windows)))]
#[doc(hidden)]
pub fn run_internal_pty_role() -> anyhow::Result<Option<i32>> {
    Ok(None)
}

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    /// One line for the loadable-tool listing. It is shown as written (only
    /// whitespace is collapsed) and never cut, so a tool can keep a detail that
    /// its first description sentence does not carry. Without it the listing
    /// falls back to the complete `description`; `Customization::tool_summaries`
    /// overrides both. It is never serialized: request bodies, snapshots and
    /// cached tool JSON keep their exact bytes.
    #[serde(default, skip_serializing)]
    pub summary: Option<String>,
}

pub fn get_tool_definitions() -> Vec<ToolDefinition> {
    let bash_tool = shell_tool_description::bash_tool();
    let mut defs = vec![
        image_generation::definition(),
        ToolDefinition {
        summary: Some("Invoke a skill by name; its instructions come back in this turn".into()), name: "Skill".into(), description: "Invoke a skill by name. The available skills are listed in a system reminder in the conversation, one line each with an optional description and execution context. Pass the exact name from that list without a leading slash; plugin-provided skills use the plugin:skill form. args is optional and is passed to the skill's template. A skill marked context: fork runs in an isolated child conversation and returns its result. Call this tool rather than opening a skill's file yourself; the tool supplies shell context and fork isolation.".into(), input_schema:serde_json::json!({"type":"object","properties":{"skill":{"description":"Exact skill name from the reminder, without a leading slash; plugin skills use plugin:skill.","type":"string"},"args":{"description":"Optional arguments passed to the skill's template.","type":"string"}},"required":["skill"],"additionalProperties":false}) },
        notebook::definition(),
        lsp::definition(),
        // `worktree` stays dispatchable for older transcripts; the model sees Enter/ExitWorktree.
        agent_messages::definition(),
        agent_messages::list_agents_definition(),
        // `cron` stays dispatchable for older transcripts; the model sees the reference trio.
        crate::services::goal_proposals::definition(),
        crate::services::compaction_requests::definition(),
        context_search::definition(),
        ToolDefinition {
            summary: Some("Load deferred tools by their exact names so the next request carries them".into()),
            name: TOOL_LOAD_NAME.into(),
            description: "Load deferred tools by their exact, case-sensitive names. Use the names listed in the conversation's system reminder and pass them as an array, for example {\"names\":[\"ListDir\",\"WebFetch\"]}. Call the loaded tools starting with the next model request. Tool loading does not grant permission to run a tool.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "names": {
                        "type": "array",
                        "description": "Exact names of deferred tools to load, in order",
                        "items": {"type": "string", "minLength": 1},
                        "minItems": 1,
                        "maxItems": MAX_TOOL_LOAD_NAMES
                    }
                },
                "required": ["names"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            summary: Some("Execute an installed, enabled plugin command with OS user permissions".into()),
            name: "PluginRun".into(),
            description: "Execute an installed, enabled plugin command with OS user permissions. Requires execution approval.".into(),
            input_schema: serde_json::json!({"type":"object","properties":{"plugin":{"type":"string"},"command":{"type":"string"},"input":{}},"required":["plugin","command"]}),
        },
        github::definition(),
        read::definition(),
        ToolDefinition {
            summary: Some("Create a file or replace its whole contents".into()),
            name: "Write".to_string(),
            description: "Create a file or replace its whole contents with UTF-8 text. Missing parent directories are created. If the path already exists, read it in this conversation first and leave it unchanged since, or the call is refused. For a partial change to an existing file, use Edit or MultiEdit instead. The result reports the resolved path and how many bytes were written.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "file_path": { "description": "Path of the file to create or overwrite.", "type": "string" },
                    "content": { "description": "Full UTF-8 text for the file.", "type": "string" }
                },
                "required": ["file_path", "content"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            summary: Some("Replace one exact string in a workspace file".into()),
            name: "Edit".to_string(),
            description: "Change a file by replacing one exact string. If you need two or more non-adjacent edits in the same file, use MultiEdit in one call instead of sequential Edit calls; batch edits to independent files in parallel in one turn. Read the file first in this conversation and leave it unchanged since, or the call is refused. old_string must match exactly once, indentation included, unless replace_all is true. LF and CRLF compare as equal, and replacements keep the file's existing line-ending style. A miss or duplicate match returns an error and writes nothing. Exclude the line-number prefix that Read adds; match only the file text.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "file_path": { "description": "Path of the file to change.", "type": "string" },
                    "old_string": { "description": "Exact text to find, without Read's line-number prefix.", "type": "string" },
                    "new_string": { "description": "Replacement text; it must differ from old_string.", "type": "string" },
                    "replace_all": { "description": "Replace every match instead of requiring a unique one; default false.", "default": false, "type": "boolean" }
                },
                "required": ["file_path", "old_string", "new_string"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            summary: Some("Run a shell command in the session shell; background and detached runs survive the call".into()),
            name: "Bash".to_string(),
            description: bash_tool.description().to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "description": "Supply the script in the syntax of the selected shell.", "type": "string" },
                    "timeout": { "description": "Allow 1–600000 ms for a foreground wait (default 120000 ms) or 1–86400000 ms for a background run (unlimited by default).", "type": "number", "minimum": 1, "maximum": 86400000 },
                    "description": { "description": "Give the command a short label for the user, such as “Check archive integrity”.", "type": "string" },
                    "run_in_background": { "description": "Enable this to get a managed task ID immediately (default false).", "type": "boolean" },
                    "interaction": { "description": "For background input via TaskInput, choose pipe on Unix or pty on Linux or Windows (default closed stdin).", "type": "string", "enum": ["pipe", "pty"] },
                    "rows": { "description": "Set the opening PTY height to 1–500 rows (default 24).", "type": "integer", "minimum": 1, "maximum": 500, "default": 24 },
                    "cols": { "description": "Set the opening PTY width to 1–1000 columns (default 80).", "type": "integer", "minimum": 1, "maximum": 1000, "default": 80 },
                    "detach": { "description": "Start a long-running service outside this call's process tree so it survives the call; returns the PID and log paths, and you stop it explicitly (default false, cannot be combined with timeout or run_in_background).", "type": "boolean" },
                    "dangerouslyDisableSandbox": { "description": "Request execution outside the configured sandbox if policy allows it (default false).", "type": "boolean" },
                    "shell": { "description": bash_tool.shell_parameter(), "type": "string" }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            summary: Some("Run a script in the background and get notified for each matching output line".into()),
            name: "Monitor".to_string(),
            description: format!("Runs a shell script in the background for this session and streams its output to you: every stdout line becomes a notification that wakes you, so you never poll. Lines within 200 ms arrive together. Use it to watch logs, builds, CI or any condition over time: write a script that prints a line only when something you would act on happens (for example `tail -f app.log | grep --line-buffered ERROR`, or a polling loop that echoes on change). Each pipe stage must flush per line. stderr is logged, not notified. The monitor expires after timeout_ms (default 30 min, max 60 min). Set max_runtime_ms to keep watching the same live script for another timeout_ms at each deadline, up to that total runtime. A renewal can send one notification, with at most one renewal notice per minute. You are notified when the monitor exits, expires or is stopped. More than 30 output events a minute stops it, so filter tightly. It runs only while this session runs. TaskStop with its task_id stops it. For a single 'tell me when X finishes', use Bash run_in_background instead.{}", bash_tool.monitor_suffix()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "description": "The shell script to run in the background", "type": "string" },
                    "description": { "description": "Short description shown in notifications", "type": "string" },
                    "timeout_ms": { "description": "Length of one watch interval, in milliseconds", "type": "integer", "minimum": 1000, "maximum": monitor::MAX_TIMEOUT.as_millis() as u64, "default": monitor::DEFAULT_TIMEOUT.as_millis() as u64 },
                    "max_runtime_ms": { "description": "Optional total watch time in milliseconds. Must exceed timeout_ms and is capped at 24 hours. Providing it enables auto-renewal while the script is alive.", "type": "integer", "minimum": 1001, "maximum": monitor::MAX_RUNTIME.as_millis() as u64 },
                    "shell": { "description": bash_tool.shell_parameter(), "type": "string" }
                },
                "required": ["command", "description"],
                "additionalProperties": false
            }),
        },
        python::definition(),
        ToolDefinition {
            summary: Some("List the contents of a directory".into()),
            name: "ListDir".to_string(),
            description: "List the contents of a directory.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Directory path to list" }
                },
                "required": ["path"]
            }),
        },
        ToolDefinition {
            summary: Some("Read a web page and answer a prompt from it with the session model".into()),
            name: "WebFetch".to_string(),
            description: "Read a web page and answer prompt from it with the session's model. The result is that answer, not the page markup or the raw response body. http(s) only; public http is upgraded to https. Local and private addresses need the user's approval unless the session is in bypass mode. Public redirects are followed automatically, but a public page is never allowed to redirect to a local or private address. HTML becomes readable text, and text, JSON, XML and JavaScript are read as-is. Cleaned text of 20,000 characters or fewer is returned directly; only longer text is summarized by the model from prompt, and a summary that exceeds its output limit falls back to the first 20,000 characters of the page text. A failed direct fetch or a JavaScript-only shell page falls back to the Jina Reader automatically. Downloads stop at 2 MiB, answers cover up to 50,000 characters from offset, and next_offset continues a truncated page; a successful download is reused for fifteen minutes. When you need the exact response body, such as from a local JSON endpoint, run curl or a similar command through Bash instead.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": { "description": "Page address to fetch; http(s) only.", "type": "string", "format": "uri" },
                    "prompt": { "description": "Question for the session's model to answer from the page text.", "type": "string" },
                    "offset": { "description": "Zero-based character position to begin at; pass next_offset from a truncated result.", "type": "integer", "minimum": 0 }
                },
                "required": ["url", "prompt"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            summary: Some("Search the web with the provider native search, or its public adapter when configured".into()),
            name: "WebSearch".to_string(),
            description: "Use the provider's native web search, or its public adapter when configured. Each call returns at most eight results with a title, URL and snippet. allowed_domains keeps matching hosts and blocked_domains drops them; both include subdomains. The query must be non-empty and at most 2,000 characters. If the provider has no native search or has search disabled, the call fails with a settings error; do not report that as a successful search. Use the system prompt date to judge recency.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "description": "Search terms; non-empty and at most 2,000 characters.", "type": "string", "minLength": 2 },
                    "allowed_domains": { "description": "Keep only results whose host matches one of these domains or a subdomain.", "type": "array", "items": { "type": "string" } },
                    "blocked_domains": { "description": "Drop results from these domains and their subdomains.", "type": "array", "items": { "type": "string" } }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        },
        apply_patch_tool::definition(),
        ToolDefinition {
            summary: Some("Apply several exact replacements to one file in a single call".into()),
            name: "MultiEdit".to_string(),
            description: "Apply several exact replacements to one file in a single call. Prefer MultiEdit over sequential Edit calls whenever making two or more non-adjacent changes to the same file (e.g. imports + logic + tests in one call: {\"file_path\":\"src/lib.rs\",\"edits\":[{\"old_string\":\"...\",\"new_string\":\"...\"},{\"old_string\":\"...\",\"new_string\":\"...\"}]}). Edits run in order against the working text, and the file is written only if every edit succeeds; one failure returns the failing edit's error and leaves the file unchanged. Read the file first and leave it unchanged since. Each old_string must be unique unless its replace_all is true, and LF and CRLF compare as equal.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "file_path": { "type": "string", "description": "Path of the file to edit." },
                    "edits": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_string": { "type": "string" },
                                "new_string": { "type": "string" },
                                "replace_all": { "type": "boolean", "default": false }
                            }
                        }
                    }
                },
                "required": ["file_path", "edits"]
            }),
        },
        ToolDefinition {
            summary: Some("Ask the user up to four questions and wait for the answers".into()),
            name: "AskUserQuestion".to_string(),
            description: "Ask when a decision belongs to the user: the request, the code and sensible defaults leave it open, and the answer changes what you do next. The schema allows 1-4 questions per call; each question has a short header, 2-4 options with a label and a description, and an optional multiSelect flag. Put your recommended option first. If a conventional default or something you can check settles it, decide yourself, say why, and continue.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "questions": {
                        "description": "One to four questions to present, each with its own options.",
                        "type": "array", "minItems": 1, "maxItems": 4,
                        "items": {
                            "type": "object",
                            "properties": {
                                "question": { "description": "The question text shown to the user.", "type": "string" },
                                "header": { "description": "Short label for the question, at most 12 characters.", "type": "string" },
                                "options": {
                                    "description": "Two to four answer choices; the host adds a free-text answer.",
                                    "type": "array", "minItems": 2, "maxItems": 4,
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "label": { "description": "Short text the user picks.", "type": "string" },
                                            "description": { "description": "What choosing this option means or causes.", "type": "string" },
                                            "preview": { "description": "Optional content shown while this option is highlighted, such as a code snippet or mockup.", "type": "string" }
                                        },
                                        "required": ["label", "description"],
                                        "additionalProperties": false
                                    }
                                },
                                "multiSelect": { "description": "Allow more than one option to be selected.", "default": false, "type": "boolean" }
                            },
                            "required": ["question", "header", "options", "multiSelect"],
                            "additionalProperties": false
                        }
                    },
                    "answers": { "description": "Pre-filled answers keyed by question text; normally omitted.", "type": "object", "additionalProperties": { "type": "string" } },
                    "annotations": { "description": "Notes the user attached to a question's selection, keyed by question text.", "type": "object" },
                    "metadata": { "description": "Optional tracking data for the host; not shown to the user.", "type": "object", "properties": { "source": { "description": "Identifier for where the question originated.", "type": "string" } }, "additionalProperties": false }
                },
                "required": ["questions"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            summary: Some("Control the computer: move the mouse, click, type text, press keys and take screenshots".into()),
            name: "computer_use".to_string(),
            description: "Control the computer: move mouse, click, type text, press keys, take screenshots. Use this to interact with the desktop GUI. Screenshots are saved locally; results contain screenshot_path (absolute path) and mime_type, not Base64 image data.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "action_type": {
                        "type": "string",
                        "enum": ["MouseMove", "MouseClick", "MouseDown", "MouseUp", "MouseScroll", "KeyPress", "KeyDown", "KeyUp", "TypeText", "Screenshot", "Wait"],
                        "description": "The type of computer action to perform"
                    },
                    "coordinate": {
                        "type": "object",
                        "properties": {
                            "x": { "type": "integer", "description": "X coordinate (pixels)" },
                            "y": { "type": "integer", "description": "Y coordinate (pixels)" }
                        },
                        "description": "Screen coordinate for mouse actions"
                    },
                    "button": {
                        "type": "string",
                        "enum": ["Left", "Right", "Middle", "Back", "Forward"],
                        "description": "Mouse button for click actions (default: Left)"
                    },
                    "key": {
                        "type": "string",
                        "description": "Key name for keyboard actions (e.g. 'Enter', 'Tab', 'Escape', 'a', 'F1')"
                    },
                    "text": {
                        "type": "string",
                        "description": "Text to type for TypeText action"
                    },
                    "scroll_y": {
                        "type": "integer",
                        "description": "Vertical scroll amount (positive = down, negative = up)"
                    },
                    "scroll_x": {
                        "type": "integer",
                        "description": "Horizontal scroll amount (positive = right, negative = left)"
                    },
                    "duration_ms": {
                        "type": "integer",
                        "description": "Duration in milliseconds for Wait action"
                    }
                },
                "required": ["action_type"]
            }),
        },
    ];

    defs.extend(search::definitions());
    defs.extend(cron_jobs::definitions());
    defs.extend(session_worktree::definitions());
    defs.push(wakeup::definition());
    defs.push(findings::definition());

    defs.extend(tasks::definitions());
    defs.push(toolscript::definition());
    defs.extend(coordination::definitions());
    defs.extend(teams::definitions());
    defs.extend(external_agents::definitions());
    defs.extend(crate::services::plans::definitions());
    defs.extend(content_compat::definitions());
    for def in &mut defs {
        if def.name == "ListDir" {
            def.input_schema["required"] = serde_json::json!([]);
            def.input_schema["properties"]["path"]["default"] = serde_json::json!(".");
        }
    }
    // The legacy `lsp` alias stays dispatchable but is no longer advertised.
    defs
}

/// Unified tool dispatch macro — eliminates duplication between
/// execute_tool (sync) and execute_tool_async.  Only the three divergent
/// tools (Bash, WebFetch, WebSearch) are parameterised; the other 19+
/// arms are shared.
macro_rules! tool_dispatch {
    ($name:ident, $input:ident, $cwd:ident,
     Bash => $bash:expr,
     WebFetch => $webfetch:expr,
     WebSearch => $websearch:expr $(,)?) => {
        match $name {
            "NotebookEdit" => notebook::execute($input, $cwd),
            "SendMessage" => agent_messages::execute($input, $cwd),
            "team" => coordination::execute($name, $input, $cwd),
            // Retired duplicate plan entry: fail like an unknown tool, before
            // any legacy plan database is opened.
            "plan_mode" => Err(anyhow::anyhow!("Unknown tool: plan_mode")),
            "cron" => scheduled::execute($input, $cwd),
            "Config" | "brief" | "SyntheticOutput" => content_compat::execute($name, &$input, $cwd),
            TOOL_LOAD_NAME => tool_load($input),
            "ReportFindings" => findings::execute(&$input),
            "TaskCreate" | "TaskGet" | "TaskList" | "TaskUpdate" | "TaskOutput" | "TaskStop" | "TaskInput" | "TodoWrite" | "TodoRead" => tasks::execute($name, $input, $cwd),
            "Read" => read::execute($input, $cwd),
            "Write" => tool_write($input, $cwd),
            "Edit" => tool_edit($input, $cwd),
            "Bash" => $bash,
            "Glob" => search::glob($input, $cwd),
            "Grep" => search::grep($input, $cwd),
            "ListDir" => tool_list_dir($input, $cwd),
            "WebFetch" => $webfetch,
            "WebSearch" => $websearch,
            "MultiEdit" => tool_multi_edit($input, $cwd),
            "ApplyPatch" => apply_patch_tool::execute($input, $cwd),
            "AskUserQuestion" => tool_ask_user_question($input),
            "computer_use" => tool_computer_use($input),
            _ if $name.is_empty() => Ok(serde_json::json!({
                "error": "Tool call has no name; the model returned an incomplete tool call"
            })),
            _ => Err(anyhow!("Unknown tool: {}", $name)),
        }
    };
}

fn directory_input(
    name: &str,
    mut input: serde_json::Value,
    cwd: &str,
) -> Result<serde_json::Value> {
    if name == "ListDir" {
        let path = input["path"]
            .as_str()
            .filter(|path| !path.trim().is_empty())
            .unwrap_or(".");
        input["path"] = serde_json::json!(resolve_read_path(path, cwd)?);
    }
    Ok(input)
}

pub fn execute_tool(name: &str, input: serde_json::Value, cwd: &str) -> Result<serde_json::Value> {
    let input = directory_input(name, input, cwd)?;
    // A monitor keeps running after this call, so it needs an async runtime
    // that outlives it; the synchronous entry point would drop it.
    anyhow::ensure!(
        name != "Monitor",
        "Monitor requires the asynchronous tool entry point"
    );
    if name == "Python" {
        return python::execute_blocking(input, cwd);
    }
    tool_dispatch!(name, input, cwd,
        Bash => tool_bash(input, cwd),
        WebFetch => tool_web_fetch_blocking(input),
        WebSearch => tool_web_search_blocking(input),
    )
}

pub async fn execute_tool_async(
    name: &str,
    input: serde_json::Value,
    cwd: &str,
) -> Result<serde_json::Value> {
    let input = directory_input(name, input, cwd)?;
    if name == "Monitor" {
        return tool_bash_async(monitor_input(input)?, cwd).await;
    }
    if name == "Python" {
        return python::execute(input, cwd).await;
    }
    if name == "computer_use" {
        return tool_computer_use_async(input).await;
    }
    if name == "LSP" || name == "lsp" {
        return lsp::execute(input, cwd).await;
    }
    if name == "worktree" {
        return worktrees::execute(input, cwd).await;
    }
    if name == "GitHub" {
        return github::execute(input, cwd).await;
    }
    if external_agents::is_external_agent(name) {
        return external_agents::execute(name, input, cwd).await;
    }
    tool_dispatch!(name, input, cwd,
        Bash => tool_bash_async(input, cwd).await,
        WebFetch => tool_web_fetch_async(input).await,
        WebSearch => tool_web_search_async(input).await,
    )
}

pub(crate) fn tool_load_names(input: &serde_json::Value) -> Result<Vec<&str>> {
    let names = input
        .as_object()
        .filter(|object| object.len() == 1)
        .and_then(|object| object.get("names"))
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            anyhow!("ToolLoad requires an object with a names array of exact tool names.")
        })?;
    anyhow::ensure!(
        (1..=MAX_TOOL_LOAD_NAMES).contains(&names.len()),
        "ToolLoad requires between 1 and {MAX_TOOL_LOAD_NAMES} names."
    );
    names
        .iter()
        .enumerate()
        .map(|(index, value)| {
            value
                .as_str()
                .filter(|name| !name.is_empty())
                .ok_or_else(|| anyhow!("ToolLoad name {} must be a non-empty string.", index + 1))
        })
        .collect()
}

/// The host-level fallback resolves definitions without marking them loaded.
/// Only the session engine can grant a definition to its next request.
fn tool_load(input: serde_json::Value) -> Result<serde_json::Value> {
    let names = tool_load_names(&input)?;
    let catalog = get_tool_definitions();
    let mut tools = Vec::new();
    let mut unknown = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for name in names.into_iter().filter(|name| seen.insert(*name)) {
        match catalog.iter().find(|tool| tool.name == name) {
            Some(tool) => tools.push(tool),
            None => unknown.push(name),
        }
    }
    anyhow::ensure!(unknown.is_empty(), "Unknown tools: {}.", unknown.join(", "));
    Ok(serde_json::json!({
        "success": false,
        "loaded_tools": [],
        "tools": tools,
        "content": "A session is required to load definitions into a model request.",
    }))
}

fn max_output_bytes() -> usize {
    crate::config::OrchestrationConfig::max_output_bytes()
}
fn default_bash_timeout_secs() -> u64 {
    crate::config::OrchestrationConfig::default_bash_timeout_secs()
}

mod dangerous_command;
mod text_edit;
pub fn is_dangerous_command(command: &str) -> bool {
    dangerous_command::matches(command)
}

#[cfg(test)]
mod command_filter_tests {
    use super::is_dangerous_command;

    #[test]
    fn powershell_candidates_follow_the_running_windows_root() {
        use std::ffi::OsStr;
        let root = OsStr::new("X:/Windows");
        let programs = OsStr::new("X:/Program Files");
        let candidates =
            super::windows_powershell_candidates("powershell", Some(root), Some(programs));
        assert_eq!(
            candidates[0],
            std::path::Path::new(root).join("System32/WindowsPowerShell/v1.0/powershell.exe")
        );
        let modern = super::windows_powershell_candidates("pwsh", Some(root), Some(programs));
        assert_eq!(modern[1], candidates[0]);
        assert!(super::windows_powershell_candidates("powershell", None, None).is_empty());
    }

    #[tokio::test]
    async fn bypass_executes_filtered_text_and_tracks_live_mode_without_argument_spoofing() {
        use crate::permissions::{AuditLogger, PermissionManager, PermissionMode};
        let manager = std::sync::Arc::new(PermissionManager::new(std::sync::Arc::new(
            AuditLogger::new(16),
        )));
        let dir = tempfile::tempdir().unwrap();
        crate::config::OrchestrationConfig::init_global().unwrap();
        // Matched evaluation primitives with harmless payloads; never shut down.
        let cases = if cfg!(windows) {
            vec![(
                "powershell",
                "Invoke-Expression \"Write-Output 'shutdown /s /t 0'\"",
            )]
        } else {
            vec![("sh", "eval \"printf '%s' 'shutdown /s /t 0'\"")]
        };
        for (shell, command) in cases {
            let input =
                serde_json::json!({"shell":shell,"command":command,"__bypass_permissions":true});
            let run = || super::tool_bash_async(input.clone(), dir.path().to_str().unwrap());
            assert_eq!(run().await.unwrap()["blocked"], true);
            super::path_scope::with_permissions(Some(manager.clone()), async {
                manager.set_mode(PermissionMode::BypassPermissions);
                let result = run().await.unwrap();
                assert_eq!(result["exit_code"], 0, "{result}");
                assert!(result["stdout"]
                    .as_str()
                    .unwrap()
                    .contains("shutdown /s /t 0"));
                manager.set_mode(PermissionMode::AskPermissions);
                assert_eq!(run().await.unwrap()["blocked"], true);
            })
            .await;
            assert_eq!(run().await.unwrap()["blocked"], true);
        }
    }

    #[test]
    fn ordinary_http_requests_are_not_unconditionally_blocked() {
        for command in [
            "Invoke-WebRequest -Uri 'http://127.0.0.1:5199/' -UseBasicParsing -TimeoutSec 8",
            "iwr -Uri https://example.com",
            "curl --data '{\"query\":\"status\"}' http://127.0.0.1:8080/query",
            "curl -F file=@fixture.txt http://127.0.0.1:8080/upload",
        ] {
            assert!(!is_dangerous_command(command), "{command}");
        }
        for command in ["rm -rf /", "format c:", "curl | bash", "nc -e /bin/sh"] {
            assert!(is_dangerous_command(command), "{command}");
        }
    }
}

fn limit_output(output: &str) -> String {
    crate::performance::head_tail_preview(output, max_output_bytes())
}

fn is_path_allowed(resolved_path: &Path, cwd: &str) -> bool {
    let canonical = match resolved_path.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            let mut current = resolved_path.to_path_buf();
            let mut suffix = PathBuf::new();
            loop {
                if let Ok(p) = current.canonicalize() {
                    break p.join(suffix);
                }
                match current.file_name() {
                    Some(name) => {
                        suffix = PathBuf::from(name).join(suffix);
                        current = match current.parent() {
                            Some(p) => p.to_path_buf(),
                            None => return false,
                        };
                    }
                    None => return false,
                }
            }
        }
    };
    if path_scope::allows(&canonical) {
        return true;
    }
    if let Ok(cwd_canonical) = Path::new(cwd).canonicalize() {
        if canonical.starts_with(&cwd_canonical) {
            return true;
        }
    }
    if let Some(appdata) = dirs::data_dir() {
        if canonical.starts_with(&appdata) {
            return true;
        }
    }
    if let Some(local_appdata) = dirs::data_local_dir() {
        if canonical.starts_with(&local_appdata) {
            return true;
        }
    }
    false
}

/// Workspace allowlist check exposed for session-level shell cwd snapshotting.
pub(crate) fn is_shell_cwd_allowed(resolved_path: &Path, cwd: &str) -> bool {
    is_path_allowed(resolved_path, cwd)
}

fn is_windows_drive_path(path: &str) -> bool {
    let stripped = path.strip_prefix(r"\\?\").unwrap_or(path);
    let bytes = stripped.as_bytes();
    bytes.len() >= 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes.len() == 2 || matches!(bytes[2], b'/' | b'\\'))
}

fn git_bash_drive_path_to_windows(path: &str) -> Option<String> {
    let bytes = path.as_bytes();
    if bytes.len() < 2 || bytes[0] != b'/' || !bytes[1].is_ascii_alphabetic() {
        return None;
    }
    let drive = (bytes[1] as char).to_ascii_uppercase();
    if bytes.len() == 2 {
        Some(format!("{drive}:/"))
    } else if bytes[2] == b'/' {
        Some(format!("{drive}:{}", &path[2..]))
    } else {
        None
    }
}

fn normalize_path_for_compare(path: &str) -> String {
    let stripped = path.strip_prefix(r"\\?\").unwrap_or(path);
    let mut normalized = stripped.replace('\\', "/");
    if is_windows_drive_path(&normalized) {
        while normalized.len() > 3 && normalized.ends_with('/') {
            normalized.pop();
        }
        return normalized.to_ascii_lowercase();
    }
    while normalized.len() > 1 && normalized.ends_with('/') {
        normalized.pop();
    }
    normalized
}

/// Whether two working directories name the same place; the shell footer
/// writes canonical paths while the engine may have started from a symlink.
fn same_directory(a: &str, b: &str) -> bool {
    match (Path::new(a).canonicalize(), Path::new(b).canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => normalize_path_for_compare(a) == normalize_path_for_compare(b),
    }
}

/// Prefix of the one-line coaching note added to a foreground `Bash` result
/// when the command starts with a redundant `cd` into the already-active cwd.
pub(crate) const REDUNDANT_CD_NOTE_PREFIX: &str = "Bash already runs in ";

/// Prefix of the one-line coaching note added to a foreground `Bash` result
/// when the command only reads or searches files via shell utilities instead
/// of using parallel `Read`/`Grep`/`Glob` tool calls.
pub(crate) const SHELL_READ_SEARCH_NOTE_PREFIX: &str = "Use dedicated Read/Grep/Glob";

/// Prefix of the one-line coaching note added to a foreground `Bash` result
/// when a single turn splits work across multiple foreground `Bash` tool calls.
pub(crate) const BASH_BATCH_NOTE_PREFIX: &str = "Avoid splitting multiple Bash calls in one turn";

/// Prefix of the one-line coaching note added to a foreground `Bash` result
/// when consecutive turns emit one foreground `Bash` call at a time.
pub(crate) const SERIAL_BASH_NOTE_PREFIX: &str = "Avoid serial single-Bash turns";

/// Returns true when `command` only uses shell read/search utilities (`cat`,
/// `sed -n`, `head`, `tail`, `grep`, `rg`, `find`, `ls`, `wc`) — optionally
/// separated by `echo`, `;`, `&&`, or a leading `cd` — with no file writes or
/// real build/git commands. Commands like `cargo test | tail -40`,
/// `git diff | grep`, or `sed -i` return `false`.
pub(crate) fn is_pure_shell_read_or_search(command: &str) -> bool {
    // Any output redirection or in-place sed edit is not a pure read/search.
    if command.contains('>') {
        return false;
    }
    let mut saw_read_or_search = false;
    for raw_part in command.split([';', '\n']) {
        for sub in raw_part.split("&&") {
            let segment = sub.trim();
            if segment.is_empty() {
                continue;
            }
            // Ignore separator echoes and leading `cd` segments.
            let first = segment.split_whitespace().next().unwrap_or_default();
            if matches!(first, "echo" | "printf" | "cd") {
                continue;
            }
            // Every pipe stage in this segment must be a read/search/filter utility.
            for (idx, pipe_stage) in segment.split('|').enumerate() {
                let stage = pipe_stage.trim();
                let mut words = stage.split_whitespace();
                let Some(cmd) = words.next() else {
                    return false;
                };
                if idx == 0 {
                    match cmd {
                        "cat" | "head" | "tail" | "grep" | "rg" | "find" | "ls" | "wc" => {
                            saw_read_or_search = true;
                        }
                        "sed" => {
                            // Only treat `sed -n` (line-range printing) as read; never `sed -i`.
                            let args: Vec<&str> = words.collect();
                            if args.iter().any(|a| *a == "-i" || a.starts_with("-i"))
                                || !args.iter().any(|a| *a == "-n")
                            {
                                return false;
                            }
                            saw_read_or_search = true;
                        }
                        _ => return false,
                    }
                } else if !matches!(
                    cmd,
                    "head" | "tail" | "grep" | "rg" | "sed" | "wc" | "sort" | "uniq" | "cut"
                ) {
                    return false;
                }
            }
        }
    }
    saw_read_or_search
}

/// Returns true when `command` begins with a leading `cd <target>` that
/// resolves to `current_cwd` itself (so the `cd` is a no-op).
///
/// Only recognizes unambiguous literal targets (unquoted, single-quoted, or
/// double-quoted without shell expansion) followed by `&&`, `;`, newline, or
/// end-of-command. Subshells, variables, `cd -`, bare `cd`, and moves into a
/// different directory return `false`.
pub(crate) fn has_redundant_leading_cd(command: &str, current_cwd: &str) -> bool {
    let trimmed = command.trim_start();
    let Some(rest) = trimmed.strip_prefix("cd") else {
        return false;
    };
    let Some(first) = rest.chars().next() else {
        return false;
    };
    if !matches!(first, ' ' | '\t') {
        return false;
    }
    let mut rest = rest.trim_start_matches([' ', '\t']);
    if let Some(after_dash) = rest.strip_prefix("--") {
        if after_dash.starts_with([' ', '\t']) {
            rest = after_dash.trim_start_matches([' ', '\t']);
        }
    }
    if let Some(after_d) = rest.strip_prefix("/d").or_else(|| rest.strip_prefix("/D")) {
        if after_d.starts_with([' ', '\t']) {
            let next = after_d.trim_start_matches([' ', '\t']);
            if !next.is_empty()
                && !next.starts_with("&&")
                && !next.starts_with(';')
                && !next.starts_with('\n')
                && !next.starts_with('\r')
            {
                rest = next;
            }
        }
    }
    if rest.is_empty() || rest.starts_with('-') {
        return false;
    }
    let (target, after) = match rest.as_bytes()[0] {
        b'\'' => {
            let inner = &rest[1..];
            let Some(end) = inner.find('\'') else {
                return false;
            };
            (&inner[..end], &inner[end + 1..])
        }
        b'"' => {
            let inner = &rest[1..];
            let Some(end) = inner.find('"') else {
                return false;
            };
            let candidate = &inner[..end];
            let allow_backslash = is_windows_drive_path(candidate);
            if candidate.contains(['$', '`']) || (!allow_backslash && candidate.contains('\\')) {
                return false;
            }
            (candidate, &inner[end + 1..])
        }
        _ => {
            let end = rest
                .find(|c: char| c.is_ascii_whitespace() || matches!(c, '&' | ';' | '|' | '>' | '<'))
                .unwrap_or(rest.len());
            let candidate = &rest[..end];
            let allow_backslash = is_windows_drive_path(candidate);
            let check_chars = if allow_backslash {
                candidate.strip_prefix(r"\\?\").unwrap_or(candidate)
            } else {
                candidate
            };
            if candidate.is_empty()
                || check_chars.contains(['$', '`', '~', '*', '?'])
                || (!allow_backslash && candidate.contains('\\'))
            {
                return false;
            }
            (candidate, &rest[end..])
        }
    };
    if target.is_empty() {
        return false;
    }
    let after = after.trim_start_matches([' ', '\t']);
    if !(after.is_empty()
        || after.starts_with('\n')
        || after.starts_with('\r')
        || after.starts_with(';')
        || after.starts_with("&&"))
    {
        return false;
    }
    if target == "." || target == "./" || target == ".\\" {
        return true;
    }
    let normalized_current = if cfg!(windows) || is_windows_drive_path(current_cwd) {
        git_bash_drive_path_to_windows(current_cwd).unwrap_or_else(|| current_cwd.to_owned())
    } else {
        current_cwd.to_owned()
    };
    let normalized_target = if cfg!(windows) || is_windows_drive_path(&normalized_current) {
        git_bash_drive_path_to_windows(target).unwrap_or_else(|| target.to_owned())
    } else {
        target.to_owned()
    };
    let resolved =
        if Path::new(&normalized_target).is_absolute() || is_windows_drive_path(&normalized_target)
        {
            PathBuf::from(&normalized_target)
        } else {
            Path::new(&normalized_current).join(&normalized_target)
        };
    let Some(resolved) = resolved.to_str() else {
        return false;
    };
    same_directory(resolved, &normalized_current)
}

#[cfg(test)]
#[test]
fn redundant_leading_cd_detection_cases() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().canonicalize().unwrap();
    let sub = project.join("sub");
    let spaced = project.join("my repo");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::create_dir_all(&spaced).unwrap();
    let cwd = project.to_str().unwrap();
    let sub_str = sub.to_str().unwrap();
    let spaced_str = spaced.to_str().unwrap();

    // Redundant leading `cd` into the already-active cwd.
    assert!(has_redundant_leading_cd(&format!("cd {cwd} && git status"), cwd));
    assert!(has_redundant_leading_cd(&format!("  cd {cwd}/ && ls"), cwd));
    assert!(has_redundant_leading_cd(&format!("cd -- {cwd}; pwd"), cwd));
    assert!(has_redundant_leading_cd(&format!("cd '{spaced_str}' && ls"), spaced_str));
    assert!(has_redundant_leading_cd(&format!("cd \"{spaced_str}\"\nls"), spaced_str));
    assert!(has_redundant_leading_cd("cd . && git diff", cwd));
    assert!(has_redundant_leading_cd("cd ./ ; git diff", cwd));
    assert!(has_redundant_leading_cd(&format!("cd {cwd}"), cwd));

    // Legitimate directory changes or ambiguous shell syntax must not match.
    assert!(!has_redundant_leading_cd(&format!("cd {sub_str} && ls"), cwd));
    assert!(!has_redundant_leading_cd("cd sub && ls", cwd));
    assert!(!has_redundant_leading_cd("cd .. && ls", cwd));
    assert!(!has_redundant_leading_cd("cd - && ls", cwd));
    assert!(!has_redundant_leading_cd("cd && ls", cwd));
    assert!(!has_redundant_leading_cd("cd $REPO && ls", cwd));
    assert!(!has_redundant_leading_cd("cd \"$REPO\" && ls", cwd));
    assert!(!has_redundant_leading_cd(&format!("(cd {cwd} && ls)"), cwd));
    assert!(!has_redundant_leading_cd(&format!("git status && cd {cwd}"), cwd));
    assert!(!has_redundant_leading_cd(&format!("echo 'cd {cwd}'"), cwd));
    assert!(!has_redundant_leading_cd(&format!("cd {cwd} || exit 1"), cwd));
}

#[cfg(test)]
#[test]
fn redundant_leading_cd_windows_and_git_bash_variants() {
    let win_backslash = r"D:\code\project\HuLaSpark\HuLa";
    let win_slash = "D:/code/project/HuLaSpark/HuLa";

    // Git Bash /d/... and Windows D:/... or D:\... variants all match current sub-repo cwd.
    assert!(has_redundant_leading_cd(
        "cd /d/code/project/HuLaSpark/HuLa && git status",
        win_backslash
    ));
    assert!(has_redundant_leading_cd(
        "cd /D/code/project/HuLaSpark/HuLa/ && git status",
        win_backslash
    ));
    assert!(has_redundant_leading_cd(
        "cd '/d/code/project/HuLaSpark/HuLa' && cargo check",
        win_slash
    ));
    assert!(has_redundant_leading_cd(
        "cd \"/d/code/project/HuLaSpark/HuLa\" && cargo check",
        win_backslash
    ));
    assert!(has_redundant_leading_cd(
        "cd D:/code/project/HuLaSpark/HuLa && git status",
        win_backslash
    ));
    assert!(has_redundant_leading_cd(
        r"cd D:\code\project\HuLaSpark\HuLa && git status",
        win_slash
    ));
    assert!(has_redundant_leading_cd(
        r#"cd "D:\code\project\HuLaSpark\HuLa" && git status"#,
        win_backslash
    ));
    assert!(has_redundant_leading_cd(
        r"cd /d D:\code\project\HuLaSpark\HuLa && git status",
        win_backslash
    ));

    // Different sub-repo or parent directory must not match.
    assert!(!has_redundant_leading_cd(
        "cd /d/code/project/HuLaSpark && git status",
        win_backslash
    ));
    assert!(!has_redundant_leading_cd(
        "cd /d/code/project/HuLaSpark/Other && git status",
        win_backslash
    ));
    assert!(!has_redundant_leading_cd(
        "cd D:/code/project/HuLaSpark/Other && git status",
        win_slash
    ));
}

#[cfg(test)]
#[tokio::test]
async fn bash_subrepo_redundant_cd_emits_note_even_without_hint_flag() {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().to_path_buf();
    let subrepo = workspace.join("HuLa");
    std::fs::create_dir_all(&subrepo).unwrap();
    let workspace_str = workspace.to_str().unwrap();
    let subrepo_str = subrepo.to_str().unwrap();
    let state = workspace.join("cwd-state");

    // At workspace root without __hint_redundant_cd, redundant cd stays quiet.
    let at_root = tool_bash_async(
        serde_json::json!({
            "command": "cd . && echo ok",
            "shell": "bash",
            "__cwd_state": state.to_string_lossy()
        }),
        workspace_str,
    )
    .await
    .unwrap();
    assert!(at_root.get("note").is_none(), "{at_root}");

    // Move into sub-repo `HuLa` so persisted shell-cwd != workspace_root.
    let moved = tool_bash_async(
        serde_json::json!({
            "command": "cd HuLa && echo entered",
            "shell": "bash",
            "__cwd_state": state.to_string_lossy()
        }),
        workspace_str,
    )
    .await
    .unwrap();
    assert_eq!(moved["success"], true, "{moved}");
    assert!(
        same_directory(moved["cwd"].as_str().unwrap_or_default(), subrepo_str),
        "expected cwd in subrepo, got: {moved}"
    );

    // Inside sub-repo (shell-cwd != workspace_root), a redundant leading cd
    // emits the explicit hint even without __hint_redundant_cd.
    let subrepo_bash_path = if cfg!(windows) {
        let stripped = subrepo_str.strip_prefix(r"\\?\").unwrap_or(subrepo_str);
        let bytes = stripped.as_bytes();
        if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            let drive = (bytes[0] as char).to_ascii_lowercase();
            format!("/{drive}{}", stripped[2..].replace('\\', "/"))
        } else {
            stripped.replace('\\', "/")
        }
    } else {
        subrepo_str.to_owned()
    };
    let redundant = tool_bash_async(
        serde_json::json!({
            "command": format!("cd '{subrepo_bash_path}' && echo still-here"),
            "shell": "bash",
            "__cwd_state": state.to_string_lossy()
        }),
        workspace_str,
    )
    .await
    .unwrap();
    let note = redundant["note"].as_str().unwrap_or_default();
    assert!(
        note.starts_with(REDUNDANT_CD_NOTE_PREFIX) && note.contains("HuLa"),
        "expected explicit redundant-cd hint for sub-repo shell-cwd, got: {redundant}"
    );
}

#[cfg(test)]
#[tokio::test]
async fn bash_defer_cwd_persist_freezes_batch_start_cwd() {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let workspace = dir.path().to_path_buf();
    let subrepo = workspace.join("HuLa");
    std::fs::create_dir_all(&subrepo).unwrap();
    let workspace_str = workspace.to_str().unwrap();
    let subrepo_str = subrepo.to_str().unwrap();
    let state = workspace.join("cwd-state");

    // Two sibling foreground Bash calls in the same turn both run `cd HuLa && ...`
    // starting from the frozen `batch_start_cwd` (`workspace_str`) with deferred persist.
    let first = tool_bash_async(
        serde_json::json!({
            "command": "cd HuLa && echo first",
            "shell": "bash",
            "__cwd_state": state.to_string_lossy(),
            "__cwd_start": workspace_str,
            "__cwd_defer_persist": true
        }),
        workspace_str,
    )
    .await
    .unwrap();
    assert_eq!(first["success"], true, "{first}");
    assert!(
        !state.exists(),
        "deferred persist must not write state mid-batch"
    );
    assert_eq!(first["__cwd_commit"]["changed"], true, "{first}");
    assert_eq!(first["__cwd_commit"]["success"], true, "{first}");
    assert!(
        same_directory(
            first["__cwd_commit"]["cwd"].as_str().unwrap_or_default(),
            subrepo_str
        ),
        "{first}"
    );

    let second = tool_bash_async(
        serde_json::json!({
            "command": "cd HuLa && echo second",
            "shell": "bash",
            "__cwd_state": state.to_string_lossy(),
            "__cwd_start": workspace_str,
            "__cwd_defer_persist": true
        }),
        workspace_str,
    )
    .await
    .unwrap();
    assert_eq!(
        second["success"], true,
        "second sibling `cd HuLa` must succeed from frozen batch_start_cwd: {second}"
    );
    assert!(!state.exists());
}

#[cfg(test)]
#[test]
fn pure_shell_read_or_search_detection_cases() {
    assert!(is_pure_shell_read_or_search("sed -n '1,100p' src/lib.rs"));
    assert!(is_pure_shell_read_or_search(
        "sed -n '1,50p' a.rs; echo '==='; sed -n '60,120p' b.rs"
    ));
    assert!(is_pure_shell_read_or_search("grep -rn \"foo\" src/ | head -30"));
    assert!(is_pure_shell_read_or_search("cd /repo && grep -n \"bar\" a.rs"));
    assert!(is_pure_shell_read_or_search("cat src/lib.rs"));
    assert!(is_pure_shell_read_or_search("ls crates/core/src"));
    assert!(is_pure_shell_read_or_search("wc -l src/lib.rs"));
    assert!(is_pure_shell_read_or_search(
        "cd /repo && ls src && grep -n foo a.rs"
    ));

    assert!(!is_pure_shell_read_or_search("cargo test 2>&1 | tail -40"));
    assert!(!is_pure_shell_read_or_search("cargo test"));
    assert!(!is_pure_shell_read_or_search("git diff | grep foo"));
    assert!(!is_pure_shell_read_or_search("git status && ls"));
    assert!(!is_pure_shell_read_or_search("sed -i '' 's/a/b/' a.rs"));
    assert!(!is_pure_shell_read_or_search("cat a.rs > b.rs"));
    assert!(!is_pure_shell_read_or_search("codegraph explore foo | head -20"));
    assert!(!is_pure_shell_read_or_search("codegraph explore foo"));
    assert!(!is_pure_shell_read_or_search("git status --short"));
}

#[cfg(all(test, unix))]
#[tokio::test]
async fn bash_adds_redundant_cd_note_only_when_requested_for_same_cwd() {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().canonicalize().unwrap();
    let sub = project.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let cwd = project.to_str().unwrap();
    let sub_str = sub.to_str().unwrap();
    let state = project.join("cwd-state");

    // Without __hint_redundant_cd, no coaching note is attached.
    let quiet = tool_bash_async(
        serde_json::json!({
            "command": format!("cd {cwd} && pwd -P"),
            "__cwd_state": state.to_string_lossy()
        }),
        cwd,
    )
    .await
    .unwrap();
    assert!(quiet.get("note").is_none(), "{quiet}");

    // With __hint_redundant_cd, redundant leading cd gets a concise note.
    let hinted = tool_bash_async(
        serde_json::json!({
            "command": format!("cd {cwd} && pwd -P"),
            "__cwd_state": state.to_string_lossy(),
            "__hint_redundant_cd": true
        }),
        cwd,
    )
    .await
    .unwrap();
    let note = hinted["note"].as_str().unwrap_or_default();
    assert!(
        note.starts_with(REDUNDANT_CD_NOTE_PREFIX) && note.contains(cwd),
        "expected redundant-cd note, got: {hinted}"
    );

    // Moving into a different directory never triggers the redundant-cd note.
    let moved = tool_bash_async(
        serde_json::json!({
            "command": format!("cd {sub_str} && pwd -P"),
            "__cwd_state": state.to_string_lossy(),
            "__hint_redundant_cd": true
        }),
        cwd,
    )
    .await
    .unwrap();
    assert!(moved.get("note").is_none(), "{moved}");

    // Once persisted cwd is `sub`, a redundant `cd sub_str` is detected against `sub`.
    let hinted_sub = tool_bash_async(
        serde_json::json!({
            "command": format!("cd {sub_str} && pwd -P"),
            "__cwd_state": state.to_string_lossy(),
            "__hint_redundant_cd": true
        }),
        cwd,
    )
    .await
    .unwrap();
    let sub_note = hinted_sub["note"].as_str().unwrap_or_default();
    assert!(
        sub_note.starts_with(REDUNDANT_CD_NOTE_PREFIX) && sub_note.contains(sub_str),
        "expected redundant-cd note for persisted sub cwd, got: {hinted_sub}"
    );
}

#[cfg(all(test, unix))]
#[tokio::test]
async fn bash_adds_redundant_cd_and_read_search_combined_note() {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().canonicalize().unwrap();
    std::fs::write(project.join("a.rs"), "fn foo() {}\n").unwrap();
    let cwd = project.to_str().unwrap();
    let state = project.join("cwd-state");

    let combined = tool_bash_async(
        serde_json::json!({
            "command": format!("cd {cwd} && grep -n foo a.rs"),
            "__cwd_state": state.to_string_lossy(),
            "__hint_redundant_cd": true
        }),
        cwd,
    )
    .await
    .unwrap();
    let note = combined["note"].as_str().unwrap_or_default();
    assert!(
        note.contains(REDUNDANT_CD_NOTE_PREFIX) && note.contains(SHELL_READ_SEARCH_NOTE_PREFIX),
        "expected combined redundant-cd + read/search note, got: {combined}"
    );
    assert_eq!(
        note,
        format!(
            "{REDUNDANT_CD_NOTE_PREFIX}{cwd}; omit redundant leading 'cd'. {SHELL_READ_SEARCH_NOTE_PREFIX} tools in parallel instead of Bash cat/sed/grep."
        )
    );
}

#[cfg(all(test, unix))]
#[tokio::test]
async fn bash_adds_redundant_cd_batch_hint_note() {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().canonicalize().unwrap();
    let cwd = project.to_str().unwrap();
    let state = project.join("cwd-state");

    let batched = tool_bash_async(
        serde_json::json!({
            "command": "pwd -P",
            "__cwd_state": state.to_string_lossy(),
            "__hint_batch_bash": true
        }),
        cwd,
    )
    .await
    .unwrap();
    let note = batched["note"].as_str().unwrap_or_default();
    assert!(
        note.starts_with(BASH_BATCH_NOTE_PREFIX),
        "expected batch-bash note, got: {batched}"
    );

    let combined = tool_bash_async(
        serde_json::json!({
            "command": format!("cd {cwd} && pwd -P"),
            "__cwd_state": state.to_string_lossy(),
            "__hint_redundant_cd": true,
            "__hint_batch_bash": true
        }),
        cwd,
    )
    .await
    .unwrap();
    let combined_note = combined["note"].as_str().unwrap_or_default();
    assert!(
        combined_note.contains(REDUNDANT_CD_NOTE_PREFIX)
            && combined_note.contains(BASH_BATCH_NOTE_PREFIX),
        "expected combined redundant-cd + batch-bash note, got: {combined}"
    );

    let serial = tool_bash_async(
        serde_json::json!({
            "command": "pwd -P",
            "__cwd_state": state.to_string_lossy(),
            "__hint_serial_bash": true
        }),
        cwd,
    )
    .await
    .unwrap();
    let serial_note = serial["note"].as_str().unwrap_or_default();
    assert!(
        serial_note.starts_with(SERIAL_BASH_NOTE_PREFIX),
        "expected serial-bash note, got: {serial}"
    );

    let combined_serial = tool_bash_async(
        serde_json::json!({
            "command": format!("cd {cwd} && pwd -P"),
            "__cwd_state": state.to_string_lossy(),
            "__hint_redundant_cd": true,
            "__hint_serial_bash": true
        }),
        cwd,
    )
    .await
    .unwrap();
    let combined_serial_note = combined_serial["note"].as_str().unwrap_or_default();
    assert!(
        combined_serial_note.contains(REDUNDANT_CD_NOTE_PREFIX)
            && combined_serial_note.contains(SERIAL_BASH_NOTE_PREFIX),
        "expected combined redundant-cd + serial-bash note, got: {combined_serial}"
    );
}

#[cfg(all(test, unix))]
#[tokio::test]
async fn bash_keeps_its_directory_and_resets_outside_the_project() {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().canonicalize().unwrap();
    std::fs::create_dir(project.join("sub")).unwrap();
    let cwd = project.to_str().unwrap();
    let state = project.join("cwd-state");
    let run = |command: &str| serde_json::json!({"command":command,"__cwd_state":state.to_string_lossy()});
    let initial = tool_bash_async(run("pwd -P"), cwd).await.unwrap();
    assert_eq!(initial["cwd"], serde_json::json!(cwd));
    let moved = tool_bash_async(run("cd sub"), cwd).await.unwrap();
    assert_eq!(
        moved["cwd"].as_str().unwrap(),
        project.join("sub").to_str().unwrap()
    );
    let out = tool_bash_async(run("pwd -P"), cwd).await.unwrap();
    assert!(out.get("cwd").is_none(), "{out}");
    assert_eq!(
        out["stdout"].as_str().unwrap().trim(),
        project.join("sub").to_str().unwrap()
    );
    // Resume/compaction forces a single cwd reminder even when unchanged.
    let reminded = tool_bash_async(
        serde_json::json!({"command":"pwd -P","__cwd_state":state.to_string_lossy(),"__report_cwd":true}),
        cwd,
    )
    .await
    .unwrap();
    assert_eq!(
        reminded["cwd"].as_str().unwrap(),
        project.join("sub").to_str().unwrap()
    );
    // Exiting early keeps the previous directory.
    let early = tool_bash_async(run("cd /; exit 3"), cwd).await.unwrap();
    assert!(early.get("cwd").is_none(), "{early}");
    assert!(tool_bash_async(run("pwd -P"), cwd).await.unwrap()["stdout"]
        .as_str()
        .unwrap()
        .contains("sub"));
    let reset = tool_bash_async(run("cd /"), cwd).await.unwrap();
    assert_eq!(reset["note"], format!("Shell cwd was reset to {cwd}"));
    assert_eq!(reset["cwd"], serde_json::json!(cwd));
    assert_eq!(
        tool_bash_async(run("pwd -P"), cwd).await.unwrap()["stdout"]
            .as_str()
            .unwrap()
            .trim(),
        cwd
    );
    // Without the engine's state file every call starts in the project.
    let plain = tool_bash_async(serde_json::json!({"command":"cd sub && pwd"}), cwd)
        .await
        .unwrap();
    assert!(plain["stdout"].as_str().unwrap().contains("sub"));
    assert_eq!(
        tool_bash_async(serde_json::json!({"command":"pwd -P"}), cwd)
            .await
            .unwrap()["stdout"]
            .as_str()
            .unwrap()
            .trim(),
        cwd
    );
}

#[cfg(all(test, unix))]
#[tokio::test]
async fn concurrent_foreground_bash_calls_preserve_saved_cwd() {
    crate::config::OrchestrationConfig::init_global().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().canonicalize().unwrap();
    let sub = project.join("sub");
    std::fs::create_dir(&sub).unwrap();
    let cwd = project.to_str().unwrap();
    let state = project.join("cwd-state");
    let run = |command: &str| serde_json::json!({"command":command,"__cwd_state":state.to_string_lossy()});
    let moved = tool_bash_async(run("cd sub"), cwd).await.unwrap();
    assert_eq!(moved["cwd"], serde_json::json!(sub.to_str().unwrap()));

    let handles: Vec<_> = (0..8)
        .map(|_| tool_bash_async(run("pwd -P"), cwd))
        .collect();
    for result in futures::future::try_join_all(handles).await.unwrap() {
        assert!(result.get("cwd").is_none(), "{result}");
        assert_eq!(
            result["stdout"].as_str().unwrap().trim(),
            sub.to_str().unwrap()
        );
    }
}

/// A batch file captures errorlevel on a separate parsed line before bookkeeping.
fn cmd_cwd_footer(command: &str, state: &str) -> String {
    format!("@echo off\r\n{command}\r\nset \"__dcl_rc=%errorlevel%\"\r\ncd > \"{state}\"\r\nexit /b %__dcl_rc%\r\n")
}

/// PowerShell keeps its directory too; runs where pwsh is installed.
#[cfg(all(test, unix))]
#[tokio::test]
async fn powershell_keeps_its_directory_and_exit_codes() {
    if find_shell("pwsh").is_none() {
        return;
    }
    crate::config::OrchestrationConfig::init_global().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().canonicalize().unwrap();
    std::fs::create_dir(project.join("sub")).unwrap();
    let cwd = project.to_str().unwrap();
    let state = project.join("cwd-state");
    let run = |command: &str| serde_json::json!({"command":command,"shell":"pwsh","__cwd_state":state.to_string_lossy()});
    tool_bash_async(run("Set-Location sub"), cwd).await.unwrap();
    let out = tool_bash_async(run("(Get-Location).ProviderPath"), cwd)
        .await
        .unwrap();
    assert_eq!(
        out["stdout"].as_str().unwrap().trim(),
        project.join("sub").to_str().unwrap(),
        "{out}"
    );
    // A native command's exit code survives the directory bookkeeping.
    let failed = tool_bash_async(run("sh -c 'exit 3'"), cwd).await.unwrap();
    assert_eq!(failed["exit_code"], 3, "{failed}");
    let reset = tool_bash_async(run("Set-Location /"), cwd).await.unwrap();
    assert_eq!(reset["note"], format!("Shell cwd was reset to {cwd}"));
    assert_eq!(
        tool_bash_async(run("(Get-Location).ProviderPath"), cwd)
            .await
            .unwrap()["stdout"]
            .as_str()
            .unwrap()
            .trim(),
        cwd
    );
}

#[cfg(test)]
#[test]
fn search_results_follow_domain_filters() {
    let results = serde_json::json!({"results":[{"url":"https://docs.rs/a"},{"url":"https://www.docs.rs/b"},{"url":"https://example.com/c"},{"url":"not a url"}]});
    let urls = |input: serde_json::Value| {
        let mut value = results.clone();
        filter_search_domains(&mut value, &input);
        value["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["url"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        urls(serde_json::json!({"allowed_domains":["docs.rs"]})),
        ["https://docs.rs/a", "https://www.docs.rs/b"]
    );
    assert_eq!(
        urls(serde_json::json!({"blocked_domains":["www.docs.rs","example.com"]})),
        ["https://docs.rs/a", "not a url"]
    );
    assert_eq!(urls(serde_json::json!({})).len(), 4);
}

/// Plan and Auto can inspect files anywhere without expanding writable roots.
pub(super) fn resolve_read_path(file_path: &str, cwd: &str) -> Result<String> {
    if path_scope::external_reads() {
        let path = Path::new(file_path);
        return Ok(if path.is_absolute() {
            path.to_path_buf()
        } else {
            Path::new(cwd).join(path)
        }
        .to_string_lossy()
        .into_owned());
    }
    resolve_path(file_path, cwd)
}

pub fn resolve_path(file_path: &str, cwd: &str) -> Result<String> {
    let resolved = if Path::new(file_path).is_absolute() {
        PathBuf::from(file_path)
    } else {
        Path::new(cwd).join(file_path)
    };
    if !is_path_allowed(&resolved, cwd) {
        return Err(anyhow!(
            "Path is outside allowed directories: {}",
            resolved.display()
        ));
    }
    Ok(resolved.to_string_lossy().to_string())
}

fn tool_write(input: serde_json::Value, cwd: &str) -> Result<serde_json::Value> {
    let file_path = input["file_path"]
        .as_str()
        .ok_or_else(|| anyhow!("file_path required"))?;
    let content = input["content"]
        .as_str()
        .ok_or_else(|| anyhow!("content required"))?;
    let path = resolve_path(file_path, cwd)?;

    if let Some(parent) = Path::new(&path).parent() {
        fs::create_dir_all(parent)?;
    }

    let before = file_change::capture(Path::new(&path));
    let bytes_written = utf8::write_utf8(Path::new(&path), content)?;

    Ok(serde_json::json!({
        "success": true,
        "content": format!("Successfully wrote to {}", path),
        "file_change": file_change::completed(Path::new(&path),before),
        "bytes_written": bytes_written
    }))
}

fn tool_edit(input: serde_json::Value, cwd: &str) -> Result<serde_json::Value> {
    let file_path = input["file_path"]
        .as_str()
        .ok_or_else(|| anyhow!("file_path required"))?;
    let old_string = input["old_string"]
        .as_str()
        .ok_or_else(|| anyhow!("old_string required"))?;
    let new_string = input["new_string"]
        .as_str()
        .ok_or_else(|| anyhow!("new_string required"))?;
    let replace_all = input["replace_all"].as_bool().unwrap_or(false);
    let path = resolve_path(file_path, cwd)?;

    let content = fs::read_to_string(&path)?;
    let before = file_change::snapshot(&content);

    let (new_content, replacements) =
        match text_edit::replace(&content, old_string, new_string, replace_all) {
            Ok(result) => result,
            Err(error) => {
                return Ok(
                    serde_json::json!({"success":false,"error":error.to_string(),"replacements":0}),
                )
            }
        };
    fs::write(&path, new_content.as_bytes())?;

    Ok(serde_json::json!({
        "success": true,
        "file_change": file_change::completed(Path::new(&path),before),
        "content": format!("Successfully replaced {} occurrence(s) in {}", replacements, path),
        "replacements": replacements
    }))
}

fn tool_multi_edit(input: serde_json::Value, cwd: &str) -> Result<serde_json::Value> {
    let file_path = input["file_path"]
        .as_str()
        .ok_or_else(|| anyhow!("file_path required"))?;
    let edits = input["edits"]
        .as_array()
        .ok_or_else(|| anyhow!("edits array required"))?;
    let path = resolve_path(file_path, cwd)?;

    let content = fs::read_to_string(&path)?;
    let before = file_change::snapshot(&content);
    let mut new_content = content.clone();
    let mut total_replacements = 0;
    for (index, edit) in edits.iter().enumerate() {
        let old = edit["old_string"]
            .as_str()
            .ok_or_else(|| anyhow!("edit {} old_string required", index + 1))?;
        let new = edit["new_string"]
            .as_str()
            .ok_or_else(|| anyhow!("edit {} new_string required", index + 1))?;
        match text_edit::replace(
            &new_content,
            old,
            new,
            edit["replace_all"].as_bool().unwrap_or(false),
        ) {
            Ok((updated, count)) => {
                new_content = updated;
                total_replacements += count;
            }
            Err(error) => {
                return Ok(
                    serde_json::json!({"success":false,"error":format!("Edit {}: {error}", index + 1),"replacements":0}),
                )
            }
        }
    }
    fs::write(&path, new_content.as_bytes())?;

    Ok(serde_json::json!({
        "success": true,
        "file_change": file_change::completed(Path::new(&path),before),
        "content": format!("Successfully applied {} edits to {}", total_replacements, path),
        "replacements": total_replacements
    }))
}

fn tool_ask_user_question(input: serde_json::Value) -> Result<serde_json::Value> {
    let question = input["question"]
        .as_str()
        .ok_or_else(|| anyhow!("question required"))?;
    let options = input["options"]
        .as_array()
        .ok_or_else(|| anyhow!("options array required"))?;

    if options.is_empty() {
        return Ok(serde_json::json!({
            "content": "Question must have at least one option",
            "is_error": true
        }));
    }

    let description = input["description"].as_str().unwrap_or("");
    let multi_select = input["multiSelect"].as_bool().unwrap_or(false);

    let options_list: Vec<serde_json::Value> = options
        .iter()
        .map(|opt| {
            serde_json::json!({
                "label": opt["label"].as_str().unwrap_or(""),
                "description": opt["description"].as_str().unwrap_or("")
            })
        })
        .collect();

    Ok(serde_json::json!({
        "type": "ask_user_question",
        "question": question,
        "description": description,
        "options": options_list,
        "multiSelect": multi_select,
        "content": format!("Waiting for user response to: {}", question),
        "requires_user_input": true
    }))
}

fn tool_computer_use(input: serde_json::Value) -> Result<serde_json::Value> {
    sync_runtime::run(tool_computer_use_async(input))
}

async fn tool_computer_use_async(input: serde_json::Value) -> Result<serde_json::Value> {
    use crate::computer_use::{
        ComputerAction, ComputerActionType, ComputerUseConfig, ComputerUseManager, MouseButton,
        ScreenCoordinate,
    };

    let action_type_str = input["action_type"]
        .as_str()
        .ok_or_else(|| anyhow!("action_type required"))?;

    let action_type = match action_type_str {
        "MouseMove" => ComputerActionType::MouseMove,
        "MouseClick" => ComputerActionType::MouseClick,
        "MouseDown" => ComputerActionType::MouseDown,
        "MouseUp" => ComputerActionType::MouseUp,
        "MouseScroll" => ComputerActionType::MouseScroll,
        "KeyPress" => ComputerActionType::KeyPress,
        "KeyDown" => ComputerActionType::KeyDown,
        "KeyUp" => ComputerActionType::KeyUp,
        "TypeText" => ComputerActionType::TypeText,
        "Screenshot" => ComputerActionType::Screenshot,
        "Wait" => ComputerActionType::Wait,
        _ => {
            return Ok(
                serde_json::json!({ "error": format!("Unknown action_type: {}", action_type_str), "is_error": true }),
            )
        }
    };

    let coordinate = input.get("coordinate").and_then(|c| {
        Some(ScreenCoordinate {
            x: c.get("x")?.as_i64()? as i32,
            y: c.get("y")?.as_i64()? as i32,
        })
    });

    let button = input["button"].as_str().and_then(|b| match b {
        "Left" => Some(MouseButton::Left),
        "Right" => Some(MouseButton::Right),
        "Middle" => Some(MouseButton::Middle),
        "Back" => Some(MouseButton::Back),
        "Forward" => Some(MouseButton::Forward),
        _ => None,
    });

    let key = input["key"].as_str().map(|s| s.to_string());
    let text = input["text"].as_str().map(|s| s.to_string());
    let scroll_y = input["scroll_y"].as_i64().map(|v| v as i32);
    let scroll_x = input["scroll_x"].as_i64().map(|v| v as i32);
    let duration_ms = input["duration_ms"].as_u64();

    let action = ComputerAction {
        action_type,
        coordinate,
        button,
        key,
        text,
        scroll_y,
        scroll_x,
        duration_ms,
    };

    let manager = ComputerUseManager::new(ComputerUseConfig::default());

    let result = manager.execute_action(action).await?;
    let screenshot = match result.screenshot.as_deref().filter(|data| !data.is_empty()) {
        Some(data) => {
            let directory = crate::data_layout::DataLayout::from_root(
                &crate::runtime::resolve_default_data_dir()?,
            )
            .screenshots();
            Some(screenshot_file::save(&directory, data)?)
        }
        None => None,
    };
    if action_type_str == "Screenshot" && result.success && screenshot.is_none() {
        return Err(anyhow!("Screenshot capture returned no image"));
    }

    Ok(serde_json::json!({
        "success": result.success,
        "action": action_type_str,
        "screenshot_path": screenshot.as_ref().map(|(path, _)| path),
        "mime_type": screenshot.as_ref().map(|(_, mime)| mime),
        "error": result.error,
        "duration_ms": result.duration_ms,
    }))
}

/// bash and zsh can make a pipeline report an earlier command's failure, so
/// `failing | head` no longer looks successful. `sh` (dash) has no pipefail.
fn pipeline_failure_flag(flag: &str, shell: &str, wsl: bool) -> bool {
    flag == "-c" && !wsl && matches!(shell, "bash" | "zsh")
}

/// Monitor is a Bash `run_in_background` call with a different finishing step.
/// The model's input becomes the Bash-shaped input the shell path expects;
/// `__monitor_timeout_ms` tells that path to start a monitor instead.
fn monitor_input(mut input: serde_json::Value) -> Result<serde_json::Value> {
    if input.get("detach").and_then(serde_json::Value::as_bool) == Some(true) {
        anyhow::bail!("Monitor does not accept detach: it already runs in the background");
    }
    let max_timeout_ms = monitor::MAX_TIMEOUT.as_millis() as u64;
    let timeout_ms = match input.get("timeout_ms").filter(|value| !value.is_null()) {
        Some(value) => value
            .as_u64()
            .filter(|ms| (1_000..=max_timeout_ms).contains(ms))
            .ok_or_else(|| anyhow!("Monitor timeout_ms must be between 1000 and 3600000"))?,
        None => monitor::DEFAULT_TIMEOUT.as_millis() as u64,
    };
    let max_runtime_ms = input.get("max_runtime_ms").filter(|value| !value.is_null());
    if let Some(value) = max_runtime_ms {
        let limit = monitor::MAX_RUNTIME.as_millis() as u64;
        anyhow::ensure!(
            value
                .as_u64()
                .is_some_and(|ms| ms > timeout_ms && ms <= limit),
            "Monitor max_runtime_ms must be an integer greater than timeout_ms and at most {limit}"
        );
    }
    input["run_in_background"] = serde_json::json!(true);
    input["__monitor_timeout_ms"] = serde_json::json!(timeout_ms);
    Ok(input)
}

async fn tool_bash_async(input: serde_json::Value, cwd: &str) -> Result<serde_json::Value> {
    let command = input["command"]
        .as_str()
        .ok_or_else(|| anyhow!("command required"))?;
    // Milliseconds, like the reference CLI. Background tasks may run longer.
    let background = input["run_in_background"].as_bool().unwrap_or(false);
    // A detached command is an unmanaged service: it must outlive this call.
    let detach = input["detach"].as_bool().unwrap_or(false);
    anyhow::ensure!(!(detach && background), "detach and run_in_background are mutually exclusive: run_in_background is a managed task, detach is an unmanaged service");
    anyhow::ensure!(
        !(detach && input["timeout"].is_number()),
        "A detached command is not waited for, so timeout has no effect; remove it"
    );
    // A managed stdin pipe is an explicit opt-in and belongs to a background
    // task; ordinary and foreground calls keep their historical null stdin.
    let pty_size = pty_bash::validate(&input)?;
    let interaction = match input.get("interaction").filter(|value| !value.is_null()) {
        None => background_shell::Interaction::None,
        Some(value) if value == "pipe" => {
            anyhow::ensure!(
                background,
                "Bash interaction:\"pipe\" needs run_in_background:true: the pipe belongs to a managed background task"
            );
            background_shell::Interaction::StdinPipe
        }
        Some(value) if value == "pty" => background_shell::Interaction::None,
        Some(other) => anyhow::bail!("Bash interaction must be pipe or pty; got {other}"),
    };
    let timeout_ms = match input.get("timeout").filter(|value| !value.is_null()) {
        Some(value) => value
            .as_f64()
            .filter(|ms| ms.is_finite() && *ms >= 1.0)
            .ok_or_else(|| anyhow!("Bash timeout must be a positive number of milliseconds"))?
            as u64,
        None => default_bash_timeout_secs() * 1000,
    };
    let max_ms = if background { 86_400_000 } else { 600_000 };
    anyhow::ensure!(
        timeout_ms <= max_ms,
        "Bash timeout must be at most {max_ms} milliseconds"
    );
    let timeout = std::time::Duration::from_millis(timeout_ms);
    let settings = background_shell::shell_settings()?;
    let resolved = settings.resolve(input["shell"].as_str(), false)?;
    let shell_type = resolved.name.as_str();
    #[cfg(windows)]
    if pty_size.is_some() {
        // Reject before WSL path translation/launch or cmd raw_arg building.
        // Command::get_args cannot describe which Windows arguments were raw.
        anyhow::ensure!(
            resolved.wsl.is_none(),
            "ConPTY managed tasks do not support WSL process-tree cleanup; no process was started"
        );
        anyhow::ensure!(resolved.command_flag != "/C",
            "ConPTY managed tasks do not support cmd raw command-line quoting yet; select PowerShell/pwsh or Git Bash. No process was started");
    }

    if !path_scope::bypass_permissions()
        && !path_scope::approved_auto_command(command)
        && is_dangerous_command(command)
    {
        return Ok(serde_json::json!({
            "error": "Command blocked by security filter: dangerous command detected",
            "is_error": true,
            "blocked": true
        }));
    }

    let shell = resolved.program;
    let flag = resolved.command_flag.to_string();

    let actual_shell = shell_type;
    // The result names the request next to the executable that actually ran, so
    // the model renderer can report a silent fallback (for example pwsh being
    // served by powershell.exe) without exposing the path on every call.
    let requested_shell = input["shell"].as_str().map(str::to_owned);
    // A managed shell records this in the task, so a later TaskGet can compare
    // it with `shell_path` and report the same fallback.
    let display_shell = requested_shell.as_deref().unwrap_or(actual_shell);
    let annotate = |mut result: serde_json::Value| {
        result["shell_path"] = serde_json::json!(shell);
        if result.get("shell").is_some() {
            if let Some(requested) = &requested_shell {
                result["shell"] = serde_json::json!(requested);
            }
        }
        result
    };
    if resolved.wsl.is_some()
        && input
            .get("__sandbox")
            .is_some_and(|policy| policy.is_object())
    {
        return Err(anyhow!("The configured sandbox cannot isolate WSL commands; select a supported shell or change the sandbox configuration explicitly"));
    }
    let mut cmd = match input.get("__sandbox").filter(|policy| policy.is_object()) {
        Some(policy) if input["dangerouslyDisableSandbox"] == true => {
            anyhow::ensure!(policy["allow_unsandboxed"] != false, "Running commands outside the sandbox is disabled by sandbox.allowUnsandboxedCommands");
            Command::new(&shell)
        }
        Some(policy) => {
            let (program, args) = sandbox::wrap(policy, &shell)?;
            let mut cmd = Command::new(program);
            cmd.args(args);
            cmd
        }
        // Android: run the shell inside the installed Ubuntu environment.
        None if !cfg!(windows) && matches!(shell_type.to_lowercase().as_str(), "bash" | "sh") => {
            match linux_env::active() {
                Some(env) => env.bash_command(cwd),
                None => Command::new(&shell),
            }
        }
        None => Command::new(&shell),
    };
    // Dropping a cancelled/expired turn must also stop its shell command.
    cmd.kill_on_drop(true);
    // A shell is a separate session: the worker's host-private variables must
    // not reach it, nor the nested CLI or test runner it may start.
    crate::services::background_sessions::strip_internal_env(&mut cmd);

    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);

    anyhow::ensure!(
        Path::new(cwd).is_dir(),
        "Shell working directory does not exist: {cwd}"
    );
    let safe_cwd = cwd.to_owned();
    // Like the reference CLI, a foreground shell keeps its working directory
    // between calls; the engine passes a per-session state file.
    let shell_name = shell_type.to_lowercase();
    let powershell = flag == "-Command";
    let cmd_shell = flag == "/C" && cfg!(windows);
    let cwd_state = input["__cwd_state"]
        .as_str()
        .map(PathBuf::from)
        .map(|state| {
            if let Some(wsl) = &resolved.wsl {
                use std::hash::{Hash, Hasher};
                let mut hash = std::collections::hash_map::DefaultHasher::new();
                wsl.distribution.hash(&mut hash);
                state.with_extension(format!("wsl-{:016x}", hash.finish()))
            } else {
                state
            }
        })
        .filter(|_| {
            !background
                && (powershell
                    || cmd_shell
                    || (flag == "-c"
                        && matches!(shell_name.as_str(), "bash" | "sh" | "zsh" | "wsl")))
        });
    let saved_cwd = |state: &PathBuf| {
        fs::read_to_string(state)
            .ok()
            .map(|text| text.trim_start_matches('\u{feff}').trim().to_owned())
    };
    let defer_cwd_persist = input["__cwd_defer_persist"].as_bool().unwrap_or(false);
    let batch_start_cwd = if resolved.wsl.is_none() {
        input["__cwd_start"]
            .as_str()
            .map(str::trim)
            .filter(|saved| {
                !saved.is_empty()
                    && Path::new(saved).is_dir()
                    && is_path_allowed(Path::new(saved), cwd)
            })
            .map(str::to_owned)
    } else {
        None
    };
    let saved_before = cwd_state.as_ref().and_then(saved_cwd);
    let report_initial = cwd_state.is_some()
        && !saved_before.as_deref().is_some_and(|saved| {
            Path::new(saved).is_dir() && is_path_allowed(Path::new(saved), cwd)
        });
    let safe_cwd = match batch_start_cwd.as_deref().or(saved_before.as_deref()) {
        Some(saved) if Path::new(saved).is_dir() && is_path_allowed(Path::new(saved), cwd) => {
            saved.to_owned()
        }
        _ => safe_cwd,
    };

    let raw_command = command.to_owned();
    let command = if flag == "-Command" {
        cmd.args(["-NoProfile", "-NonInteractive"]);
        utf8::powershell_command(command)
    } else if flag == "/C" && cfg!(windows) {
        utf8::cmd_command(command)
    } else {
        command.to_string()
    };
    let command = if !cfg!(windows) || resolved.wsl.is_some() {
        match background_shell::session_environment()? {
            Some(script)
                if matches!(
                    shell_type.to_lowercase().as_str(),
                    "bash" | "sh" | "zsh" | "wsl"
                ) =>
            {
                format!(
                    "{{\n{script}\n}} && eval '{}'",
                    command.replace('\'', "'\"'\"'")
                )
            }
            // Upstream injects the session environment only through its bash
            // provider; other shells keep working without it.
            Some(_) => {
                tracing::warn!(
                    shell = %shell_type,
                    "Session environment files apply to bash, sh and zsh; running without one"
                );
                command
            }
            None => command,
        }
    } else {
        command
    };
    struct CwdCaptureFile(PathBuf);
    impl Drop for CwdCaptureFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }
    let cwd_capture = cwd_state.as_ref().map(|state| {
        if let Some(parent) = state.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let name = state
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("shell-cwd");
        CwdCaptureFile(
            state.with_file_name(format!(".{name}.tmp.{}", uuid::Uuid::new_v4())),
        )
    });
    let command = match &cwd_capture {
        // Each call writes its ending directory to a per-call temp file so
        // concurrent read-only foreground calls never truncate the shared state.
        // A command that exits early leaves the temp file unwritten and keeps
        // the previous directory.
        Some(capture) if resolved.wsl.is_some() => {
            let path =
                crate::services::wsl::linux_path(resolved.wsl.as_ref().unwrap(), &capture.0)
                    .await?;
            format!("{command}\n__dcl_status=$?\nwslpath -w \"$PWD\" > '{}' 2>/dev/null\nexit $__dcl_status", path.replace('\'', "'\"'\"'"))
        }
        Some(capture) if powershell => format!(
            "{command}\n$__dcl_ok = $?; $__dcl_code = $global:LASTEXITCODE\ntry {{ [System.IO.File]::WriteAllText('{}', (Get-Location).ProviderPath) }} catch {{}}\nif (-not $__dcl_ok) {{ if ($__dcl_code) {{ exit $__dcl_code }} else {{ exit 1 }} }}",
            capture.0.to_string_lossy().replace('\'', "''")
        ),
        Some(capture) if cmd_shell => cmd_cwd_footer(&command, &capture.0.to_string_lossy()),
        // Git Bash on Windows prints C:/... with -W; elsewhere -P is the path.
        Some(capture) => format!(
            "{command}\n__dcl_status=$?\n{} > '{}' 2>/dev/null\nexit $__dcl_status",
            if cfg!(windows) { "{ pwd -W 2>/dev/null || pwd -P; }" } else { "pwd -P" },
            capture.0.to_string_lossy().replace('\'', "'\"'\"'")
        ),
        None => command,
    };
    // Keep the batch file alive until the foreground command finishes.
    let mut cmd_script = None;
    if cmd_shell && cwd_state.is_some() {
        use std::io::Write;
        let mut script = tempfile::Builder::new().suffix(".cmd").tempfile()?;
        script.write_all(command.as_bytes())?;
        cmd_script = Some(script);
    }
    let mut wsl_guard = None;
    if let Some(settings) = &resolved.wsl {
        let (launched, guard) =
            crate::services::wsl::launch(settings, Path::new(&safe_cwd), Some(&command))
                .await?
                .command();
        cmd = launched;
        wsl_guard = Some(guard);
    }
    #[cfg(windows)]
    if resolved.wsl.is_some() {
        // The WSL launcher already contains the command and explicit distribution.
    } else if flag == "/C" {
        // cmd uses its own quoting rules; normal Windows argument escaping adds
        // literal backslashes before quotes in nested PowerShell file paths.
        let argument = cmd_script
            .as_ref()
            .map(|file| format!("\"{}\"", file.path().display()))
            .unwrap_or_else(|| command.clone());
        cmd.args(["/D", "/S", "/C"])
            .raw_arg(format!("\"{argument}\""));
    } else {
        // Without this, `failing | head` reports head's success.
        if pipeline_failure_flag(&flag, &shell_name, resolved.wsl.is_some()) {
            cmd.args(["-o", "pipefail"]);
        }
        cmd.arg(&flag).arg(&command);
    }
    #[cfg(not(windows))]
    {
        if pipeline_failure_flag(&flag, &shell_name, resolved.wsl.is_some()) {
            cmd.args(["-o", "pipefail"]);
        }
        cmd.arg(&flag).arg(&command);
    }
    cmd.current_dir(&safe_cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Background commands are named by their description, or the command.
    let description = input["description"]
        .as_str()
        .filter(|text| !text.trim().is_empty())
        .unwrap_or(&command)
        .to_owned();
    #[cfg(any(target_os = "linux", windows))]
    if let Some((rows, cols)) = pty_size {
        let result = pty_bash::start(cmd, rows, cols, &description, &command)?;
        return Ok(annotate(result));
    }
    #[cfg(not(any(target_os = "linux", windows)))]
    let _ = pty_size;
    if detach {
        let directory = crate::runtime::resolve_default_data_dir()?
            .join("tasks/detached-shells")
            .join(uuid::Uuid::new_v4().to_string());
        let mut result = bash_process::start_detached(cmd, &directory, display_shell)?;
        std::fs::write(directory.join("command.txt"), command.as_bytes())?;
        result["directory"] = serde_json::json!(directory);
        Ok(annotate(result))
    } else if background {
        if let Some(timeout_ms) = input
            .get("__monitor_timeout_ms")
            .and_then(serde_json::Value::as_u64)
        {
            let result = monitor::start_with_runtime_limit(
                cmd,
                &description,
                &command,
                std::time::Duration::from_millis(timeout_ms),
                input["max_runtime_ms"]
                    .as_u64()
                    .map(std::time::Duration::from_millis),
                wsl_guard,
            )?;
            return Ok(annotate(result));
        }
        let result = background_shell::start(
            cmd,
            input
                .get("timeout")
                .filter(|value| !value.is_null())
                .map(|_| timeout),
            display_shell,
            &description,
            &command,
            wsl_guard,
            interaction,
        )?;
        Ok(annotate(result))
    } else {
        // Ctrl+B can move it to the background while it runs.
        let filtered = input.get("__sandbox").filter(|policy| {
            policy["network_rules"].is_object() && input["dangerouslyDisableSandbox"] != true
        });
        let mark = filtered.map(sandbox::blocked_mark);
        let result = background_shell::run_foreground(
            cmd,
            timeout,
            display_shell,
            &description,
            &command,
            wsl_guard,
        )
        .await?;
        let mut result = annotate(result);
        // Name refused hosts: an HTTPS client often reports only a reset.
        if let (Some(policy), Some(mark)) = (filtered, mark) {
            let hosts = sandbox::blocked_since(policy, mark);
            if let (false, Some(object)) = (hosts.is_empty(), result.as_object_mut()) {
                object.insert("sandbox_network".into(), serde_json::json!(format!("The sandbox blocked network access to {}: not in sandbox.network.allowedDomains (or listed in deniedDomains) in settings.json. Ask the user to allow the domain; do not retry with other hosts.", hosts.join(", "))));
            }
        }
        let force_report_cwd = input["__report_cwd"].as_bool().unwrap_or(false);
        let command_succeeded = result["success"].as_bool().unwrap_or(false);
        if let (Some(state), Some(capture)) = (&cwd_state, &cwd_capture) {
            let moved = saved_cwd(&capture.0).filter(|dir| !dir.is_empty());
            let reset = moved
                .as_deref()
                .is_some_and(|dir| !is_path_allowed(Path::new(dir), cwd));
            if reset {
                if !defer_cwd_persist {
                    let _ = fs::remove_file(state);
                }
                if let Some(object) = result.as_object_mut() {
                    object.insert(
                        "note".into(),
                        serde_json::json!(format!("Shell cwd was reset to {cwd}")),
                    );
                    object.insert("cwd".into(), serde_json::json!(cwd));
                    if defer_cwd_persist {
                        object.insert(
                            "__cwd_commit".into(),
                            serde_json::json!({
                                "state": state.to_string_lossy(),
                                "reset": true,
                                "changed": true,
                                "success": command_succeeded,
                            }),
                        );
                    }
                }
            } else if let Some(moved) = moved {
                let moved_changed = !same_directory(&moved, &safe_cwd);
                let changed = report_initial || moved_changed;
                if !defer_cwd_persist {
                    if changed || !state.exists() {
                        // Atomically promote the per-call capture file so concurrent
                        // readers never observe a truncated state file.
                        if fs::rename(&capture.0, state).is_err() {
                            let _ = fs::write(state, format!("{moved}\n"));
                        }
                    }
                } else if moved_changed || !state.exists() {
                    if let Some(object) = result.as_object_mut() {
                        object.insert(
                            "__cwd_commit".into(),
                            serde_json::json!({
                                "state": state.to_string_lossy(),
                                "cwd": moved,
                                "reset": false,
                                "changed": moved_changed,
                                "success": command_succeeded,
                            }),
                        );
                    }
                }
                if changed || force_report_cwd {
                    if let Some(object) = result.as_object_mut() {
                        object.insert("cwd".into(), serde_json::json!(moved));
                    }
                }
            } else if (report_initial || force_report_cwd)
                && result.as_object().is_some()
            {
                if let Some(object) = result.as_object_mut() {
                    object.insert("cwd".into(), serde_json::json!(safe_cwd));
                    if defer_cwd_persist && !state.exists() {
                        object.insert(
                            "__cwd_commit".into(),
                            serde_json::json!({
                                "state": state.to_string_lossy(),
                                "cwd": safe_cwd,
                                "reset": false,
                                "changed": false,
                                "success": command_succeeded,
                            }),
                        );
                    }
                }
            }
        }
        let hint_redundant_cd = input["__hint_redundant_cd"].as_bool().unwrap_or(false);
        let hint_batch_bash = input["__hint_batch_bash"].as_bool().unwrap_or(false);
        let hint_serial_bash = input["__hint_serial_bash"].as_bool().unwrap_or(false);
        let non_root_shell_cwd = cwd_state.is_some() && !same_directory(&safe_cwd, cwd);
        let redundant_leading_cd = (hint_redundant_cd || non_root_shell_cwd)
            && has_redundant_leading_cd(&raw_command, &safe_cwd);
        if redundant_leading_cd || hint_redundant_cd || hint_batch_bash || hint_serial_bash {
            let mut notes = Vec::new();
            if redundant_leading_cd {
                notes.push(format!(
                    "{REDUNDANT_CD_NOTE_PREFIX}{safe_cwd}; omit redundant leading 'cd'."
                ));
            }
            if hint_redundant_cd && is_pure_shell_read_or_search(&raw_command) {
                notes.push(format!(
                    "{SHELL_READ_SEARCH_NOTE_PREFIX} tools in parallel instead of Bash cat/sed/grep."
                ));
            }
            if hint_batch_bash {
                notes.push(format!(
                    "{BASH_BATCH_NOTE_PREFIX}; chain related shell commands with && or ; in one Bash call, or use parallel Read/Grep/Glob."
                ));
            } else if hint_serial_bash {
                notes.push(format!(
                    "{SERIAL_BASH_NOTE_PREFIX}; chain related shell commands in one Bash call with && or ; (or use parallel Read/Grep/Glob)."
                ));
            }
            if !notes.is_empty() {
                if let Some(object) = result.as_object_mut() {
                    object
                        .entry("note".to_owned())
                        .or_insert_with(|| serde_json::json!(notes.join(" ")));
                }
            }
        }
        Ok(result)
    }
}

// Sync wrapper: delegates to tool_bash_async via block_on.
// Single source of truth — avoids duplicating shell/danger/error logic.
fn tool_bash(input: serde_json::Value, cwd: &str) -> Result<serde_json::Value> {
    let cwd = cwd.to_owned();
    sync_runtime::run(async move { tool_bash_async(input, &cwd).await })
}

pub fn find_git_bash() -> Option<String> {
    find_shell("bash")
}

#[cfg(any(windows, test))]
fn windows_powershell_candidates(
    shell: &str,
    system_root: Option<&std::ffi::OsStr>,
    program_files: Option<&std::ffi::OsStr>,
) -> Vec<PathBuf> {
    let native = system_root.map(|root| {
        PathBuf::from(root)
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe")
    });
    let modern = program_files.map(|root| {
        PathBuf::from(root)
            .join("PowerShell")
            .join("7")
            .join("pwsh.exe")
    });
    if shell == "pwsh" {
        modern.into_iter().chain(native).collect()
    } else {
        native.into_iter().chain(modern).collect()
    }
}

pub fn find_shell(shell_name: &str) -> Option<String> {
    if cfg!(windows) && matches!(shell_name, "bash" | "sh") {
        return shell_discovery::windows_git_shell(shell_name)
            .map(|path| path.to_string_lossy().into_owned());
    }
    #[cfg(windows)]
    if matches!(shell_name, "powershell" | "pwsh") {
        // PE lives on X:, and a Windows installation can use another drive.
        // Prefer the running OS over an offline installation's C:\Windows.
        let system_root = std::env::var_os("SystemRoot").or_else(|| std::env::var_os("WINDIR"));
        let program_files = std::env::var_os("ProgramFiles");
        let candidates = windows_powershell_candidates(
            shell_name,
            system_root.as_deref(),
            program_files.as_deref(),
        );
        if let Some(path) = candidates.into_iter().find(|path| path.is_file()) {
            return Some(path.to_string_lossy().into_owned());
        }
        return lsp_servers::find_on_path(shell_name)
            .or_else(|| {
                (shell_name == "pwsh")
                    .then(|| lsp_servers::find_on_path("powershell"))
                    .flatten()
            })
            .map(|path| path.to_string_lossy().into_owned());
    }
    let candidates = match (shell_name, cfg!(target_os = "windows")) {
        ("bash", true) => vec![
            r"C:\Program Files\Git\bin\bash.exe".to_string(),
            r"C:\Program Files (x86)\Git\bin\bash.exe".to_string(),
            r"C:\Git\bin\bash.exe".to_string(),
        ],
        ("bash", false) => vec![
            format!("{}/bin/bash", std::env::var("PREFIX").unwrap_or_default()),
            "/usr/local/bin/bash".to_string(),
            "/usr/bin/bash".to_string(),
            "/bin/bash".to_string(),
        ],
        ("zsh", true) => vec![
            r"C:\Program Files\Git\usr\bin\zsh.exe".to_string(),
            r"C:\msys64\usr\bin\zsh.exe".to_string(),
        ],
        ("zsh", false) => vec![
            "/usr/local/bin/zsh".to_string(),
            "/usr/bin/zsh".to_string(),
            "/bin/zsh".to_string(),
        ],
        ("fish", true) => vec![
            r"C:\Program Files\fish\bin\fish.exe".to_string(),
            r"C:\msys64\usr\bin\fish.exe".to_string(),
        ],
        ("fish", false) => vec![
            "/usr/local/bin/fish".to_string(),
            "/usr/bin/fish".to_string(),
            "/bin/fish".to_string(),
        ],
        ("powershell", true) => vec![
            r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe".to_string(),
            r"C:\Program Files\PowerShell\7\pwsh.exe".to_string(),
        ],
        ("pwsh", true) => vec![
            r"C:\Program Files\PowerShell\7\pwsh.exe".to_string(),
            r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe".to_string(),
        ],
        ("sh", false) => vec![
            // Termux keeps its tools under $PREFIX; Android itself has /system/bin/sh.
            format!("{}/bin/sh", std::env::var("PREFIX").unwrap_or_default()),
            "/system/bin/sh".to_string(),
            "/usr/local/bin/sh".to_string(),
            "/usr/bin/sh".to_string(),
            "/bin/sh".to_string(),
        ],
        _ => vec![],
    };

    for path in &candidates {
        if std::path::Path::new(path).exists() {
            return Some(path.clone());
        }
    }

    if !cfg!(target_os = "windows") {
        if let Ok(output) = std::process::Command::new("which").arg(shell_name).output() {
            let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !path.is_empty() && std::path::Path::new(&path).exists() {
                return Some(path);
            }
        }
    }

    None
}

fn tool_list_dir(input: serde_json::Value, _cwd: &str) -> Result<serde_json::Value> {
    let dir_path = input["path"]
        .as_str()
        .ok_or_else(|| anyhow!("path required"))?;

    let mut entries: Vec<serde_json::Value> = Vec::new();
    for entry in fs::read_dir(dir_path)? {
        let entry = entry?;
        let metadata = entry.metadata()?;
        entries.push(serde_json::json!({
            "name": entry.file_name().to_string_lossy(),
            "is_dir": metadata.is_dir(),
            "size": metadata.len()
        }));
    }

    Ok(serde_json::json!({ "entries": entries }))
}

/// Applies WebSearch allowed_domains/blocked_domains to `results`; a domain
/// also matches its subdomains.
pub fn filter_search_domains(result: &mut serde_json::Value, input: &serde_json::Value) {
    let list = |key: &str| -> Vec<String> {
        input[key]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|d| d.as_str())
            .map(|d| {
                d.trim()
                    .trim_start_matches("*.")
                    .trim_start_matches('.')
                    .to_ascii_lowercase()
            })
            .filter(|d| !d.is_empty())
            .collect()
    };
    let (allowed, blocked) = (list("allowed_domains"), list("blocked_domains"));
    if allowed.is_empty() && blocked.is_empty() {
        return;
    }
    let matches =
        |host: &str, domain: &str| host == domain || host.ends_with(&format!(".{domain}"));
    if let Some(results) = result.get_mut("results").and_then(|r| r.as_array_mut()) {
        results.retain(|item| {
            let Some(host) = item["url"]
                .as_str()
                .and_then(|url| reqwest::Url::parse(url).ok())
                .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
            else {
                return allowed.is_empty();
            };
            (allowed.is_empty() || allowed.iter().any(|d| matches(&host, d)))
                && !blocked.iter().any(|d| matches(&host, d))
        });
    }
}

/// The model's WebFetch: the URL policy applies before any request is made.
fn model_fetch_input(mut input: serde_json::Value) -> Result<serde_json::Value> {
    let url = input["url"]
        .as_str()
        .ok_or_else(|| anyhow!("url required"))?;
    input["url"] = serde_json::json!(web::model_url(url, path_scope::approved_local_fetch(url))?);
    Ok(input)
}
fn tool_web_fetch_blocking(input: serde_json::Value) -> Result<serde_json::Value> {
    web::fetch_blocking(model_fetch_input(input)?)
}
async fn tool_web_fetch_async(input: serde_json::Value) -> Result<serde_json::Value> {
    web::fetch(model_fetch_input(input)?).await
}
/// WebFetch with the configured transport proxy, after the same URL policy.
pub(crate) async fn execute_web_fetch_async(
    input: serde_json::Value,
    proxy: Option<&str>,
) -> Result<serde_json::Value> {
    web::fetch_with_proxy(model_fetch_input(input)?, proxy).await
}
fn tool_web_search_blocking(_input: serde_json::Value) -> Result<serde_json::Value> {
    Err(anyhow!("WebSearch requires a conversation provider and its search settings. Start search from a conversation."))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToolExecutionStatus {
    Running,
    Completed,
    Canceled,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolExecutionInfo {
    pub task_id: String,
    pub tool_name: String,
    pub status: ToolExecutionStatus,
    pub result: Option<serde_json::Value>,
    pub error: Option<String>,
}

pub struct ToolExecutionManager {
    executions: Arc<RwLock<HashMap<String, tokio::sync::oneshot::Sender<()>>>>,
}

impl ToolExecutionManager {
    pub fn new() -> Self {
        Self {
            executions: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn execute_with_cancel(
        &self,
        task_id: String,
        tool_name: &str,
        input: serde_json::Value,
        cwd: &str,
    ) -> Result<serde_json::Value> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.executions.write().await.insert(task_id.clone(), tx);

        let result = tokio::select! {
            _ = rx => {
                Ok(serde_json::json!({
                    "error": "Tool execution canceled",
                    "is_error": true,
                    "canceled": true
                }))
            }
            res = execute_tool_async(tool_name, input, cwd) => res
        };

        self.executions.write().await.remove(&task_id);
        result
    }

    pub async fn cancel_execution(&self, task_id: &str) -> bool {
        if let Some(tx) = self.executions.write().await.remove(task_id) {
            let _ = tx.send(());
            true
        } else {
            false
        }
    }

    pub async fn is_running(&self, task_id: &str) -> bool {
        self.executions.read().await.contains_key(task_id)
    }

    pub async fn get_running_tasks(&self) -> Vec<String> {
        self.executions.read().await.keys().cloned().collect()
    }
}

async fn tool_web_search_async(input: serde_json::Value) -> Result<serde_json::Value> {
    tool_web_search_blocking(input)
}

pub(crate) async fn execute_public_web_search(
    input: serde_json::Value,
) -> Result<serde_json::Value> {
    web::search(input).await
}

#[cfg(test)]
mod directory_edit_regressions {
    use super::*;
    use serde_json::json;

    #[test]
    fn removed_git_tools_are_unknown_and_never_dispatched() {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().to_str().unwrap();
        for name in [
            "git_status",
            "git_diff",
            "git_log",
            "git_add",
            "git_commit",
            "smart_project_scan",
        ] {
            let error = execute_tool(name, json!({"path":cwd}), cwd)
                .unwrap_err()
                .to_string();
            assert_eq!(error, format!("Unknown tool: {name}"));
            assert!(
                get_tool_definitions().iter().all(|tool| tool.name != name),
                "{name} must not be in the catalog"
            );
        }
    }

    #[test]
    fn git_commands_run_through_bash_after_the_native_tools_are_removed() {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().to_str().unwrap();
        let result = execute_tool(
            "Bash",
            json!({"command":"git init -q && git status --porcelain"}),
            cwd,
        )
        .unwrap();
        assert_eq!(result["success"], true, "{result}");
        assert_eq!(result["exit_code"], 0, "{result}");
        // The old native tool is rejected instead of being routed to Git.
        assert!(execute_tool("git_status", json!({"path":cwd}), cwd).is_err());
    }

    #[tokio::test]
    async fn omitted_directory_uses_session_workspace() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("sentinel.txt"), "hello").unwrap();
        let cwd = root.path().to_str().unwrap();
        for input in [json!({}), json!({"path":"."}), json!({"path":null})] {
            let list = execute_tool("ListDir", input.clone(), cwd).unwrap();
            assert!(list["entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["name"] == "sentinel.txt"));
        }
    }

    #[test]
    fn multi_edit_matches_lf_preserves_crlf_and_rolls_back_failure() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file.txt");
        fs::write(&path, "one\r\ntwo\r\n").unwrap();
        let input = json!({"file_path":"file.txt","edits":[{"old_string":"one\ntwo\n","new_string":"three\nfour\n"}]});
        assert_eq!(
            tool_multi_edit(input, root.path().to_str().unwrap()).unwrap()["success"],
            true
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "three\r\nfour\r\n");
        let input = json!({"file_path":"file.txt","edits":[{"old_string":"three","new_string":"changed"},{"old_string":"missing","new_string":"bad"}]});
        assert_eq!(
            tool_multi_edit(input, root.path().to_str().unwrap()).unwrap()["replacements"],
            0
        );
        assert_eq!(fs::read_to_string(path).unwrap(), "three\r\nfour\r\n");
    }

    #[test]
    fn pipelines_report_earlier_failures_only_where_the_shell_supports_it() {
        assert!(pipeline_failure_flag("-c", "bash", false));
        assert!(pipeline_failure_flag("-c", "zsh", false));
        // dash, cmd and PowerShell have no -o pipefail, and WSL builds its own command line.
        assert!(!pipeline_failure_flag("-c", "sh", false));
        assert!(!pipeline_failure_flag("-Command", "powershell", false));
        assert!(!pipeline_failure_flag("/C", "cmd", false));
        assert!(!pipeline_failure_flag("-c", "bash", true));
    }

    #[tokio::test]
    async fn a_failing_upstream_is_not_hidden_by_a_later_reader() {
        crate::config::OrchestrationConfig::init_global().unwrap();
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().to_str().unwrap();
        let failed = execute_tool_async(
            "Bash",
            json!({"command":"false | head -1","shell":"bash"}),
            cwd,
        )
        .await
        .unwrap();
        assert_eq!(failed["success"], false, "{failed}");
        assert_ne!(failed["exit_code"], 0, "{failed}");

        let passed = execute_tool_async(
            "Bash",
            json!({"command":"printf 'a\\nb\\n' | head -1","shell":"bash"}),
            cwd,
        )
        .await
        .unwrap();
        assert_eq!(passed["success"], true, "{passed}");
        assert_eq!(passed["stdout"].as_str().unwrap().trim(), "a");
    }
}

#[cfg(test)]
mod retired_tool_tests {
    #[test]
    fn catalog_excludes_retired_memory_tools() {
        let names = super::get_tool_definitions()
            .into_iter()
            .map(|tool| tool.name)
            .collect::<Vec<_>>();
        for retired in ["memory_search", "memory_ingest"] {
            assert!(!names.iter().any(|name| name == retired), "{retired}: {names:?}");
        }
    }
}

#[cfg(test)]
mod summary_tests {
    /// The loadable-tool listing shows `summary`; a missing one falls back to the
    /// whole description, so every advertised built-in must carry one.
    #[test]
    fn every_advertised_tool_carries_a_listing_summary() {
        let missing = crate::native_engine::tool_loop::builtin_tool_catalog()
            .into_iter()
            .filter(|tool| tool.summary.as_deref().is_none_or(|text| text.trim().is_empty()))
            .map(|tool| tool.name)
            .collect::<Vec<_>>();
        assert!(missing.is_empty(), "tools without a summary: {missing:?}");
    }
}
