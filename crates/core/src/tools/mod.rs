mod sync_runtime;
pub(crate) mod screenshot_file;
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::RwLock;

pub mod macros;
mod notebook;
mod read;
mod search;
pub mod tasks;
mod lsp;

/// Whether a plugin language server declaration is usable by the LSP tool.
pub fn lsp_config_is_valid(server: &serde_json::Value) -> bool {
    lsp::validate_configuration(server).is_ok()
}
pub mod lsp_servers;
pub mod external_agents;
pub use lsp::{configure as configure_lsp_servers, shutdown_all as shutdown_lsp_servers, status as lsp_status};
pub(crate) use lsp::passive_feedback as lsp_passive_feedback;
pub(crate) use lsp::validate_configuration as validate_lsp_configuration;
mod worktrees;
pub mod agent_messages;
pub mod coordination;
pub mod teams;
pub mod scheduled;
pub mod cron_jobs;
pub mod session_worktree;
pub mod wakeup;
pub mod sandbox;
mod findings;
mod content_compat;
mod utf8;
mod bash_process;
pub mod background_shell;
pub(crate) mod path_scope;
pub mod file_change;
mod web;
pub(crate) mod provider_search;
pub use provider_search::search as search_provider;

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x08000000;
// ---- memory tool stubs (dispatched to MemEx at runtime by tool_loop) ----

fn tool_memory_search(input: serde_json::Value) -> Result<serde_json::Value> {
    Ok(serde_json::json!({
        "stub": true,
        "message": "memory_search should be intercepted by ToolLoopExecutor",
        "query": input["query"],
        "results": []
    }))
}

fn tool_memory_ingest(input: serde_json::Value) -> Result<serde_json::Value> {
    Ok(serde_json::json!({
        "stub": true,
        "message": "memory_ingest should be intercepted by ToolLoopExecutor",
        "content": input["content"],
        "stored": false
    }))
}



#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

pub fn get_tool_definitions() -> Vec<ToolDefinition> {
    let mut defs = vec![
        ToolDefinition { name: "Skill".into(), description: "Invoke a skill.\n\nA skill is a packaged set of instructions the user or project has set up for a particular kind of task (deploy steps, a review checklist, a repo-specific workflow). Available skills are listed in the system prompt with one-line descriptions. When the task at hand is one a listed skill covers, call this tool first — the skill's instructions load into the turn for you to follow in place of your default approach; skills with context:fork instead run in an isolated child conversation and return the finished result. Users may also ask for one by name (`/<name>`, or \"slash command\"); that's a request to invoke it.\n\n- `skill`: exact name from the listing, no leading slash. Plugin skills use `plugin:skill`.\n- `args`: optional arguments to pass through.\n\nOnly names from the listing (or that the user typed explicitly) are valid. Built-in CLI commands (`/help`, `/clear`, …) aren't skills. Do not read a skill file to bypass this tool: dynamic shell context and fork isolation run through it.".into(), input_schema:serde_json::json!({"type":"object","properties":{"skill":{"description":"The name of a skill from the available-skills list. Do not guess names.","type":"string"},"args":{"description":"Optional arguments for the skill","type":"string"}},"required":["skill"],"additionalProperties":false}) },
        notebook::definition(),
        lsp::definition(),
        // `worktree` stays dispatchable for older transcripts; the model sees Enter/ExitWorktree.
        agent_messages::definition(),
        agent_messages::list_agents_definition(),
        // `cron` stays dispatchable for older transcripts; the model sees the reference trio.
        crate::services::goal_proposals::definition(),
        ToolDefinition { name: "ToolSearch".into(), description: "Search the currently registered built-in tool names, descriptions and schemas. MCP tools are supplied separately by the host.".into(), input_schema: serde_json::json!({"type":"object","properties":{"query":{"type":"string"}},"required":["query"]}) },
        ToolDefinition {
            name: "PluginRun".into(),
            description: "Execute an installed, enabled plugin command with OS user permissions. Requires execution approval.".into(),
            input_schema: serde_json::json!({"type":"object","properties":{"plugin":{"type":"string"},"command":{"type":"string"},"input":{}},"required":["plugin","command"]}),
        },
        read::definition(),
        ToolDefinition {
            name: "Write".to_string(),
            description: "Writes a file to the local filesystem, overwriting if one exists.\n\nWhen to use: creating a new file, or fully replacing one you've already Read. Overwriting an existing file you haven't Read will fail. For partial changes, use Edit instead.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "file_path": { "description": "The absolute path to the file to write (must be absolute, not relative)", "type": "string" },
                    "content": { "description": "The content to write to the file", "type": "string" }
                },
                "required": ["file_path", "content"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "Edit".to_string(),
            description: "Performs exact string replacement in a file.\n\n- You must Read the file in this conversation before editing, or the call will fail.\n- `old_string` must match the file exactly, including indentation, and be unique — the edit fails otherwise. Strip the Read line prefix (line number + tab) before matching.\n- `replace_all: true` replaces every occurrence instead.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "file_path": { "description": "The absolute path to the file to modify", "type": "string" },
                    "old_string": { "description": "The text to replace", "type": "string" },
                    "new_string": { "description": "The text to replace it with (must be different from old_string)", "type": "string" },
                    "replace_all": { "description": "Replace all occurrences of old_string (default false)", "default": false, "type": "boolean" }
                },
                "required": ["file_path", "old_string", "new_string"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "Bash".to_string(),
            description: "Executes a shell command and returns its output.\n\nThe default shell is bash (Git Bash on Windows): use Unix shell syntax there — `/dev/null` not `NUL`, forward slashes, `$VAR` not `%VAR%` or `$env:VAR`. Pass `shell` (powershell, pwsh, cmd, sh, zsh, fish) to use another shell's syntax; PowerShell input/output is UTF-8.\n\n- With bash, sh or zsh the working directory persists between calls, but prefer absolute paths; leaving the project resets it to the project directory. Shell state (env vars, functions) does not persist. Other shells start in the project directory each call.\n- Command output is displayed to you, not reliably to the user.\n- `timeout` is in milliseconds: default 120000, max 600000 for a foreground command.\n- `run_in_background` runs the command detached as a managed task: it keeps running across turns and returns a task_id. No `&` or Start-Process needed. Use TaskOutput to read its output and TaskStop to stop it.\n\n# Git\n- Interactive flags (`-i`, e.g. `git rebase -i`, `git add -i`) are not supported in this environment.\n- Use the `gh` CLI for GitHub operations (PRs, issues, API).\n- Commit or push only when the user asks. If on the default branch, branch first.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "description": "The command to execute", "type": "string" },
                    "timeout": { "description": "Optional timeout in milliseconds (max 600000 for a foreground command)", "type": "number" },
                    "description": { "description": "Clear, concise description of what this command does in active voice. Never use words like \"complex\" or \"risk\" in the description - just describe what it does.\n\nSay what the command does in plain words: do not echo the command's text, its flags, or file paths - the user reads this description, often without seeing the command.\n\nFor simple commands (git, npm, standard CLI tools), keep it brief (5-10 words):\n- ls → \"List files in current directory\"\n- git status → \"Show working tree status\"\n- npm install → \"Install package dependencies\"\n\nFor commands that are harder to parse at a glance (piped commands, obscure flags, etc.), add enough context to clarify what it does:\n- find . -name \"*.tmp\" -exec rm {} \\; → \"Find and delete all .tmp files recursively\"\n- git reset --hard origin/main → \"Discard all local changes and match remote main\"\n- curl -s url | jq '.data[]' → \"Fetch JSON from URL and extract data array elements\"", "type": "string" },
                    "run_in_background": { "description": "Set to true to run this command in the background.", "type": "boolean" },
                    "dangerouslyDisableSandbox": { "description": "Set this to true to dangerously override sandbox mode and run commands without sandboxing.", "type": "boolean" },
                    "shell": { "description": "Shell to use: bash, zsh, fish, sh, cmd, powershell, pwsh (default: bash)", "type": "string" }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
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
            name: "WebFetch".to_string(),
            description: "Fetches a URL, converts the page to readable text, and answers `prompt` against it using the session's model.\n\n- Fails on authenticated/private URLs — use an authenticated MCP tool or `gh` for those instead.\n- Fails on localhost, private addresses and other hostnames without a dot; for a local server, use curl via Bash.\n- HTTP is upgraded to HTTPS. Cross-host redirects are returned to you rather than followed; call again with the redirect URL.\n- Responses are cached for 15 minutes per URL.\n- Long pages are read in parts: when the result has next_offset, call again with it as offset.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": { "description": "The URL to fetch content from", "type": "string", "format": "uri" },
                    "prompt": { "description": "The prompt to run on the fetched content", "type": "string" },
                    "offset": { "description": "Character position in the page text to start reading from. Use it to read on through a page too long for one call, with the value the previous result gave.", "type": "integer", "minimum": 0 }
                },
                "required": ["url", "prompt"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "WebSearch".to_string(),
            description: "Search the web. Returns result blocks with titles and URLs.\n\n- The current date is given in the system prompt — use it when searching for recent information.\n- `allowed_domains` / `blocked_domains` filter results.\n- After answering from results, end with a \"Sources:\" list of the URLs you used as markdown links.\n- DeepSeek models use separate search credentials configured in System settings > Web search. If configuration or native search support is missing, relay the tool's setup instructions to the user; do not claim a search succeeded or retry unchanged requests.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "description": "The search query to use", "type": "string", "minLength": 2 },
                    "allowed_domains": { "description": "Only include search results from these domains", "type": "array", "items": { "type": "string" } },
                    "blocked_domains": { "description": "Never include search results from these domains", "type": "array", "items": { "type": "string" } }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "MultiEdit".to_string(),
            description: "Make multiple string replacements in a file at once.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "file_path": { "type": "string", "description": "The path to the file" },
                    "edits": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_string": { "type": "string" },
                                "new_string": { "type": "string" }
                            }
                        }
                    }
                },
                "required": ["file_path", "edits"]
            }),
        },
        ToolDefinition {
            name: "AskUserQuestion".to_string(),
            description: "Use this tool only when you are blocked on a decision that is genuinely the user's to make: one you cannot resolve from the request, the code, or sensible defaults.\n\nUsage notes:\n- Users will always be able to type a custom answer instead of picking an option\n- Use multiSelect: true to allow multiple answers to be selected for a question\n- If you recommend a specific option, make that the first option in the list and add \"(Recommended)\" at the end of the label\n\nReserve this for decisions where the user's answer changes what you do next — not for choices with a conventional default or facts you can verify in the codebase yourself. In those cases pick the obvious option, mention it in your response, and proceed.\n\nThe user may see only the question: make each question stand on its own. If it depends on something you found, say so briefly, and name the items you are asking about in the question itself.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "questions": {
                        "description": "Questions to ask the user (1-4 questions)",
                        "type": "array", "minItems": 1, "maxItems": 4,
                        "items": {
                            "type": "object",
                            "properties": {
                                "question": { "description": "The complete question to ask the user. Should be clear, specific, and end with a question mark.", "type": "string" },
                                "header": { "description": "Very short label displayed as a chip/tag (max 12 chars). Examples: \"Auth method\", \"Library\", \"Approach\".", "type": "string" },
                                "options": {
                                    "description": "The available choices for this question. Must have 2-4 options. Each option should be a distinct, mutually exclusive choice (unless multiSelect is enabled). There should be no 'Other' option, that will be provided automatically.",
                                    "type": "array", "minItems": 2, "maxItems": 4,
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "label": { "description": "The display text for this option that the user will see and select. Should be concise (1-5 words).", "type": "string" },
                                            "description": { "description": "Explanation of what this option means or what will happen if chosen.", "type": "string" },
                                            "preview": { "description": "Optional preview content rendered when this option is focused, such as a code snippet or ASCII mockup.", "type": "string" }
                                        },
                                        "required": ["label", "description"],
                                        "additionalProperties": false
                                    }
                                },
                                "multiSelect": { "description": "Set to true to allow the user to select multiple options instead of just one.", "default": false, "type": "boolean" }
                            },
                            "required": ["question", "header", "options", "multiSelect"],
                            "additionalProperties": false
                        }
                    },
                    "answers": { "description": "User answers collected by the permission component", "type": "object", "additionalProperties": { "type": "string" } },
                    "annotations": { "description": "Optional per-question annotations from the user (e.g., notes on preview selections). Keyed by question text.", "type": "object" },
                    "metadata": { "description": "Optional metadata for tracking and analytics purposes. Not displayed to user.", "type": "object", "properties": { "source": { "description": "Optional identifier for the source of this question.", "type": "string" } }, "additionalProperties": false }
                },
                "required": ["questions"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "git_status".to_string(),
            description: "Runs git status in the workspace directory to see changed, staged, and untracked files.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "The workspace directory path to run git status in" }
                },
                "required": ["path"]
            }),
        },
        ToolDefinition {
            name: "git_diff".to_string(),
            description: "Runs git diff to show changes in the working directory or staging area.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "The workspace directory path" },
                    "staged": { "type": "boolean", "description": "If true, show staged changes (git diff --staged)" },
                    "file": { "type": "string", "description": "Optional specific file path to diff" }
                },
                "required": ["path"]
            }),
        },
        ToolDefinition {
            name: "git_log".to_string(),
            description: "Runs git log to show commit history.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "The workspace directory path" },
                    "count": { "type": "number", "description": "Number of commits to show (default: 10)" },
                    "oneline": { "type": "boolean", "description": "Use oneline format (default: true)" }
                },
                "required": ["path"]
            }),
        },
        ToolDefinition {
            name: "git_commit".to_string(),
            description: "Stages all changes and commits them with the given message. Runs git add -A then git commit.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "The workspace directory path" },
                    "message": { "type": "string", "description": "The commit message" }
                },
                "required": ["path", "message"]
            }),
        },
        ToolDefinition {
            name: "git_add".to_string(),
            description: "Stages specific files for the next commit. Runs git add with the specified file paths.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "The workspace directory path" },
                    "files": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Array of file paths to stage"
                    }
                },
                "required": ["path", "files"]
            }),
        },
        ToolDefinition {
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
        ToolDefinition {
            name: "memory_search".to_string(),
            description: "Search your persistent memory for relevant information from previous conversations. Use this to recall important context, decisions, or facts from past interactions.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "The search query to find relevant memories" },
                    "top_k": { "type": "number", "description": "Number of results to return (default: 5)" }
                },
                "required": ["query"]
            }),
        },
        ToolDefinition {
            name: "memory_ingest".to_string(),
            description: "Store important information in persistent memory for future retrieval. Use this to remember key decisions, architecture, configurations, or any information that should persist across conversations.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "content": { "type": "string", "description": "The information to store in memory" },
                    "importance": { "type": "number", "description": "Importance score 0.0-1.0 (default: auto-estimated)" }
                },
                "required": ["content"]
            }),
        },
    ];

    defs.extend(search::definitions());
    defs.extend(cron_jobs::definitions());
    defs.extend(session_worktree::definitions());
    defs.push(wakeup::definition());
    defs.push(findings::definition());
    for macro_def in macros::get_macro_definitions() {
        defs.push(ToolDefinition {
            name: macro_def.name,
            description: macro_def.description,
            input_schema: macro_def.input_schema,
        });
    }

    defs.extend(tasks::definitions());
    defs.extend(coordination::definitions());
    defs.extend(teams::definitions());
    defs.extend(external_agents::definitions());
    defs.extend(crate::services::plans::definitions());
    defs.extend(content_compat::definitions());
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
            "team" | "plan_mode" => coordination::execute($name, $input, $cwd),
            "cron" => scheduled::execute($input, $cwd),
            "Config" | "brief" | "SyntheticOutput" => content_compat::execute($name, &$input, $cwd),
            "ToolSearch" => tool_search($input),
            "ReportFindings" => findings::execute(&$input),
            "TaskCreate" | "TaskGet" | "TaskList" | "TaskUpdate" | "TaskOutput" | "TaskStop" | "TodoWrite" | "TodoRead" => tasks::execute($name, $input, $cwd),
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
            "AskUserQuestion" => tool_ask_user_question($input),
            "git_status" => tool_git_status($input),
            "git_diff" => tool_git_diff($input),
            "git_log" => tool_git_log($input),
            "git_commit" => tool_git_commit($input),
            "git_add" => tool_git_add($input),
            "computer_use" => tool_computer_use($input),
            "memory_search" => tool_memory_search($input),
            "memory_ingest" => tool_memory_ingest($input),
            "smart_project_scan" => macros::execute_smart_project_scan($input, $cwd),
            _ => Ok(serde_json::json!({ "error": format!("Unknown tool: {}", $name) })),
        }
    };
}

pub fn execute_tool(name: &str, input: serde_json::Value, cwd: &str) -> Result<serde_json::Value> {
    tool_dispatch!(name, input, cwd,
        Bash => tool_bash(input, cwd),
        WebFetch => tool_web_fetch_blocking(input),
        WebSearch => tool_web_search_blocking(input),
    )
}

pub async fn execute_tool_async(name: &str, input: serde_json::Value, cwd: &str) -> Result<serde_json::Value> {
    if name == "computer_use" { return tool_computer_use_async(input).await; }
    if name == "LSP" || name == "lsp" { return lsp::execute(input, cwd).await; }
    if name == "worktree" { return worktrees::execute(input, cwd).await; }
    if external_agents::is_external_agent(name) { return external_agents::execute(name, input, cwd).await; }
    tool_dispatch!(name, input, cwd,
        Bash => tool_bash_async(input, cwd).await,
        WebFetch => tool_web_fetch_async(input).await,
        WebSearch => tool_web_search_async(input).await,
    )
}

fn tool_search(input: serde_json::Value) -> Result<serde_json::Value> {
    let query = input["query"].as_str().ok_or_else(|| anyhow!("query is required"))?.to_lowercase();
    let tools: Vec<_> = get_tool_definitions().into_iter().filter(|tool| tool.name.to_lowercase().contains(&query) || tool.description.to_lowercase().contains(&query)).collect();
    Ok(serde_json::json!({"tools": tools}))
}

/// UTF-8 safe truncation: finds the last valid char boundary at or before `max_bytes`.
fn truncate_utf8(s: &str, max_bytes: usize) -> usize {
    if s.len() <= max_bytes {
        return s.len();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

fn max_output_bytes() -> usize { crate::config::OrchestrationConfig::max_output_bytes() }
fn default_bash_timeout_secs() -> u64 { crate::config::OrchestrationConfig::default_bash_timeout_secs() }

pub fn is_dangerous_command(command: &str) -> bool {
    let dangerous_patterns = [
        // ── Linux destructive ──
        "rm -rf /",
        "rm -rf /*",
        "rm -rf ~",
        "rm -rf .",
        "dd if=",
        "mkfs",
        ":(){ :|:& };:",
        "> /dev/sda",
        "> /dev/hda",
        "> /dev/nvme",
        "chmod 777 /",
        "chmod -R 777",
        "chown -R",
        // ── Linux priv esc / backdoor ──
        "curl | bash",
        "curl | sh",
        "wget | bash",
        "wget | sh",
        "curl|bash",
        "curl|sh",
        "wget|bash",
        "wget|sh",
        "sudo su",
        "sudo -i",
        "passwd",
        // ── Windows destructive ──
        "format c:",
        "format d:",
        "del /s /q c:",
        "del /s /q d:",
        "rd /s /q c:",
        "rd /s /q d:",
        "remove-item -recurse -force c:",
        "remove-item -recurse -force /",
        "diskpart",
        "bcdedit",
        // ── Windows priv esc / backdoor ──
        "net user administrator",
        "net localgroup administrators",
        "add-mppreference -exclusionpath",
        "set-mppreference -disable",
        "reg delete hklm\\software",
        "reg delete hkcu\\software",
        "sc stop",
        "sc delete",
        // ── Cross-platform dangerous ──
        "eval(",
        "fork bomb",
        "shutdown",
        "restart -f",
        "docker rm -f",
        "docker system prune",
        // Remote shell primitives. Ordinary HTTP clients are not inherently
        // destructive: their authorization belongs to the permission manager.
        "nc -e",
        "/dev/tcp/",
    ];

    let lower = command.to_lowercase();
    let stripped = lower
        .replace("  ", " ")
        .replace("\\ ", " ")
        .replace("\"", "")
        .replace("'", "");

    dangerous_patterns.iter().any(|pat| stripped.contains(pat))
}

#[cfg(test)]
mod command_filter_tests {
    use super::is_dangerous_command;

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
    if output.len() > max_output_bytes() {
        format!(
            "{}\n... (output truncated, exceeded {} bytes)",
            &output[..truncate_utf8(output, max_output_bytes())],
            max_output_bytes()
        )
    } else {
        output.to_string()
    }
}

fn is_path_allowed(resolved_path: &Path, cwd: &str) -> bool {
    let canonical = match resolved_path.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            let mut current = resolved_path.to_path_buf();
            let mut suffix = PathBuf::new();
            loop {
                if let Ok(p) = current.canonicalize() { break p.join(suffix); }
                match current.file_name() {
                    Some(name) => { suffix = PathBuf::from(name).join(suffix); current = match current.parent() { Some(p) => p.to_path_buf(), None => return false }; }
                    None => return false,
                }
            }
        }
    };
    if path_scope::allows(&canonical) { return true; }
    if let Ok(cwd_canonical) = Path::new(cwd).canonicalize() {
        if canonical.starts_with(&cwd_canonical) { return true; }
    }
    if let Some(appdata) = dirs::data_dir() {
        if canonical.starts_with(&appdata) { return true; }
    }
    if let Some(local_appdata) = dirs::data_local_dir() {
        if canonical.starts_with(&local_appdata) { return true; }
    }
    false
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
    tool_bash_async(run("cd sub"), cwd).await.unwrap();
    let out = tool_bash_async(run("pwd -P"), cwd).await.unwrap();
    assert_eq!(out["stdout"].as_str().unwrap().trim(), project.join("sub").to_str().unwrap());
    // Exiting early keeps the previous directory.
    let _ = tool_bash_async(run("cd /; exit 3"), cwd).await;
    assert!(tool_bash_async(run("pwd -P"), cwd).await.unwrap()["stdout"].as_str().unwrap().contains("sub"));
    let reset = tool_bash_async(run("cd /"), cwd).await.unwrap();
    assert_eq!(reset["note"], format!("Shell cwd was reset to {cwd}"));
    assert_eq!(tool_bash_async(run("pwd -P"), cwd).await.unwrap()["stdout"].as_str().unwrap().trim(), cwd);
    // Without the engine's state file every call starts in the project.
    let plain = tool_bash_async(serde_json::json!({"command":"cd sub && pwd"}), cwd).await.unwrap();
    assert!(plain["stdout"].as_str().unwrap().contains("sub"));
    assert_eq!(tool_bash_async(serde_json::json!({"command":"pwd -P"}), cwd).await.unwrap()["stdout"].as_str().unwrap().trim(), cwd);
}

/// cmd: appends the directory bookkeeping with `&`, the weakest operator, so
/// the command parses as before. `%^errorlevel%` survives the command line's
/// first expansion and `call` expands it after the command ran, which keeps
/// the command's exit code.
fn cmd_cwd_footer(command: &str, state: &str) -> String {
    format!("{command} & call set \"__dcl_rc=%^errorlevel%\" & cd > \"{state}\" & call exit /b %^__dcl_rc%")
}

#[cfg(test)]
#[test]
fn cmd_footer_appends_without_regrouping_the_command() {
    let footer = cmd_cwd_footer("chcp 65001 >nul & cd sub && dir || echo no", r"C:\data\shell-cwd\s");
    assert_eq!(footer, r#"chcp 65001 >nul & cd sub && dir || echo no & call set "__dcl_rc=%^errorlevel%" & cd > "C:\data\shell-cwd\s" & call exit /b %^__dcl_rc%"#);
}

/// PowerShell keeps its directory too; runs where pwsh is installed.
#[cfg(all(test, unix))]
#[tokio::test]
async fn powershell_keeps_its_directory_and_exit_codes() {
    if find_shell("pwsh").is_none() { return; }
    crate::config::OrchestrationConfig::init_global().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().canonicalize().unwrap();
    std::fs::create_dir(project.join("sub")).unwrap();
    let cwd = project.to_str().unwrap();
    let state = project.join("cwd-state");
    let run = |command: &str| serde_json::json!({"command":command,"shell":"pwsh","__cwd_state":state.to_string_lossy()});
    tool_bash_async(run("Set-Location sub"), cwd).await.unwrap();
    let out = tool_bash_async(run("(Get-Location).ProviderPath"), cwd).await.unwrap();
    assert_eq!(out["stdout"].as_str().unwrap().trim(), project.join("sub").to_str().unwrap(), "{out}");
    // A native command's exit code survives the directory bookkeeping.
    let failed = tool_bash_async(run("sh -c 'exit 3'"), cwd).await.unwrap();
    assert_eq!(failed["exit_code"], 3, "{failed}");
    let reset = tool_bash_async(run("Set-Location /"), cwd).await.unwrap();
    assert_eq!(reset["note"], format!("Shell cwd was reset to {cwd}"));
    assert_eq!(tool_bash_async(run("(Get-Location).ProviderPath"), cwd).await.unwrap()["stdout"].as_str().unwrap().trim(), cwd);
}

#[cfg(test)]
#[test]
fn search_results_follow_domain_filters() {
    let results = serde_json::json!({"results":[{"url":"https://docs.rs/a"},{"url":"https://www.docs.rs/b"},{"url":"https://example.com/c"},{"url":"not a url"}]});
    let urls = |input: serde_json::Value| { let mut value = results.clone(); filter_search_domains(&mut value, &input); value["results"].as_array().unwrap().iter().map(|r| r["url"].as_str().unwrap().to_owned()).collect::<Vec<_>>() };
    assert_eq!(urls(serde_json::json!({"allowed_domains":["docs.rs"]})), ["https://docs.rs/a", "https://www.docs.rs/b"]);
    assert_eq!(urls(serde_json::json!({"blocked_domains":["www.docs.rs","example.com"]})), ["https://docs.rs/a", "not a url"]);
    assert_eq!(urls(serde_json::json!({})).len(), 4);
}

pub fn resolve_path(file_path: &str, cwd: &str) -> Result<String> {
    let resolved = if Path::new(file_path).is_absolute() { PathBuf::from(file_path) } else { Path::new(cwd).join(file_path) };
    if !is_path_allowed(&resolved, cwd) {
        return Err(anyhow!("Path is outside allowed directories: {}", resolved.display()));
    }
    Ok(resolved.to_string_lossy().to_string())
}

fn tool_write(input: serde_json::Value, cwd: &str) -> Result<serde_json::Value> {
    let file_path = input["file_path"].as_str().ok_or_else(|| anyhow!("file_path required"))?;
    let content = input["content"].as_str().ok_or_else(|| anyhow!("content required"))?;
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
    let file_path = input["file_path"].as_str().ok_or_else(|| anyhow!("file_path required"))?;
    let old_string = input["old_string"].as_str().ok_or_else(|| anyhow!("old_string required"))?;
    let new_string = input["new_string"].as_str().ok_or_else(|| anyhow!("new_string required"))?;
    let replace_all = input["replace_all"].as_bool().unwrap_or(false);
    let path = resolve_path(file_path, cwd)?;

    let content = fs::read_to_string(&path)?;
    let before = file_change::snapshot(&content);

    let (new_content, replacements) = if replace_all {
        let count = content.matches(old_string).count();
        (content.replace(old_string, new_string), count)
    } else {
        let count = content.matches(old_string).count();
        if count == 0 {
            return Ok(serde_json::json!({
                "success": false,
                "error": "old_string not found in file"
            }));
        }
        if count > 1 {
            return Ok(serde_json::json!({
                "success": false,
                "error": format!("old_string found {} times, use replace_all=true", count)
            }));
        }
        (content.replacen(old_string, new_string, 1), 1)
    };

    utf8::write_utf8(Path::new(&path), &new_content)?;

    Ok(serde_json::json!({
        "success": true,
        "file_change": file_change::completed(Path::new(&path),before),
        "content": format!("Successfully replaced {} occurrence(s) in {}", replacements, path),
        "replacements": replacements
    }))
}

fn tool_multi_edit(input: serde_json::Value, cwd: &str) -> Result<serde_json::Value> {
    let file_path = input["file_path"].as_str().ok_or_else(|| anyhow!("file_path required"))?;
    let edits = input["edits"].as_array().ok_or_else(|| anyhow!("edits array required"))?;
    let path = resolve_path(file_path, cwd)?;

    let content = fs::read_to_string(&path)?;
    let before = file_change::snapshot(&content);
    let mut new_content = content;
    let mut total_replacements = 0;
    let mut failed_edits: Vec<String> = Vec::new();

    for edit in edits {
        let old_string = edit.get("old_string").and_then(|s| s.as_str());
        let new_string = edit.get("new_string").and_then(|s| s.as_str());

        if let (Some(old), Some(new)) = (old_string, new_string) {
            if new_content.contains(old) {
                new_content = new_content.replace(old, new);
                total_replacements += 1;
            } else {
                failed_edits.push(old.to_string());
            }
        }
    }

    if !failed_edits.is_empty() {
        return Ok(serde_json::json!({
            "success": false,
            "error": format!("Some edits failed: {:?}", failed_edits),
            "replacements": total_replacements
        }));
    }

    utf8::write_utf8(Path::new(&path), &new_content)?;

    Ok(serde_json::json!({
        "success": true,
        "file_change": file_change::completed(Path::new(&path),before),
        "content": format!("Successfully applied {} edits to {}", total_replacements, path),
        "replacements": total_replacements
    }))
}

fn tool_ask_user_question(input: serde_json::Value) -> Result<serde_json::Value> {
    let question = input["question"].as_str().ok_or_else(|| anyhow!("question required"))?;
    let options = input["options"].as_array().ok_or_else(|| anyhow!("options array required"))?;

    if options.is_empty() {
        return Ok(serde_json::json!({
            "content": "Question must have at least one option",
            "is_error": true
        }));
    }

    let description = input["description"].as_str().unwrap_or("");
    let multi_select = input["multiSelect"].as_bool().unwrap_or(false);

    let options_list: Vec<serde_json::Value> = options.iter().map(|opt| {
        serde_json::json!({
            "label": opt["label"].as_str().unwrap_or(""),
            "description": opt["description"].as_str().unwrap_or("")
        })
    }).collect();

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
        _ => return Ok(serde_json::json!({ "error": format!("Unknown action_type: {}", action_type_str), "is_error": true })),
    };

    let coordinate = input
        .get("coordinate")
        .and_then(|c| {
            Some(ScreenCoordinate {
                x: c.get("x")?.as_i64()? as i32,
                y: c.get("y")?.as_i64()? as i32,
            })
        });

    let button = input["button"]
        .as_str()
        .and_then(|b| match b {
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
            let directory = crate::runtime::resolve_default_data_dir()?.join("screenshots");
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

async fn tool_bash_async(input: serde_json::Value, cwd: &str) -> Result<serde_json::Value> {
    let command = input["command"].as_str().ok_or_else(|| anyhow!("command required"))?;
    // Milliseconds, like the reference CLI. Background tasks may run longer.
    let background = input["run_in_background"].as_bool().unwrap_or(false);
    let timeout_ms = match input.get("timeout").filter(|value| !value.is_null()) {
        Some(value) => value.as_f64().filter(|ms| ms.is_finite() && *ms >= 1.0).ok_or_else(|| anyhow!("Bash timeout must be a positive number of milliseconds"))? as u64,
        None => default_bash_timeout_secs() * 1000,
    };
    let max_ms = if background { 86_400_000 } else { 600_000 };
    anyhow::ensure!(timeout_ms <= max_ms, "Bash timeout must be at most {max_ms} milliseconds");
    let timeout = std::time::Duration::from_millis(timeout_ms);
    let shell_type = input["shell"].as_str().unwrap_or("bash");

    if is_dangerous_command(command) {
        return Ok(serde_json::json!({
            "error": "Command blocked by security filter: dangerous command detected",
            "is_error": true,
            "blocked": true
        }));
    }

    let (shell, flag) = match shell_type.to_lowercase().as_str() {
        "zsh" => {
            if let Some(zsh_path) = find_shell("zsh") {
                (zsh_path, "-c".to_string())
            } else {
                return Ok(serde_json::json!({
                    "error": "zsh shell not found",
                    "is_error": true
                }));
            }
        }
        "fish" => {
            if let Some(fish_path) = find_shell("fish") {
                (fish_path, "-c".to_string())
            } else {
                return Ok(serde_json::json!({
                    "error": "fish shell not found",
                    "is_error": true
                }));
            }
        }
        "sh" => {
            if let Some(sh_path) = find_shell("sh") {
                (sh_path, "-c".to_string())
            } else {
                return Ok(serde_json::json!({
                    "error": "sh shell not found",
                    "is_error": true
                }));
            }
        }
        "cmd" => {
            if cfg!(target_os = "windows") {
                ("cmd".to_string(), "/C".to_string())
            } else {
                return Ok(serde_json::json!({
                    "error": "cmd shell only available on Windows",
                    "is_error": true
                }));
            }
        }
        "powershell" | "pwsh" => {
            if let Some(pwsh_path) = find_shell(&shell_type.to_lowercase()) {
                (pwsh_path, "-Command".to_string())
            } else {
                return Ok(serde_json::json!({
                    "error": "PowerShell not found",
                    "is_error": true
                }));
            }
        }
        _ => { // bash (default)
            if let Some(bash_path) = find_shell("bash") {
                (bash_path, "-c".to_string())
            } else if cfg!(target_os = "windows") {
                ("cmd".to_string(), "/C".to_string())
            } else {
                ("sh".to_string(), "-c".to_string())
            }
        }
    };

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
        None => Command::new(&shell),
    };
    // Dropping a cancelled/expired turn must also stop its shell command.
    cmd.kill_on_drop(true);

    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);

    let safe_cwd = if cfg!(target_os = "windows") {
        if std::path::Path::new(cwd).exists() {
            cwd.to_string()
        } else {
            std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\".to_string())
        }
    } else {
        cwd.to_string()
    };
    // Like the reference CLI, a foreground shell keeps its working directory
    // between calls; the engine passes a per-session state file.
    let shell_name = shell_type.to_lowercase();
    let powershell = flag == "-Command";
    let cmd_shell = flag == "/C" && cfg!(windows);
    let cwd_state = input["__cwd_state"].as_str().map(PathBuf::from)
        .filter(|_| !background && (powershell || cmd_shell || (flag == "-c" && matches!(shell_name.as_str(), "bash" | "sh" | "zsh"))));
    let saved_cwd = |state: &PathBuf| fs::read_to_string(state).ok().map(|text| text.trim_start_matches('\u{feff}').trim().to_owned());
    let safe_cwd = match cwd_state.as_ref().and_then(saved_cwd) {
        Some(saved) if Path::new(&saved).is_dir() && is_path_allowed(Path::new(&saved), cwd) => saved,
        _ => safe_cwd,
    };

    let command = if flag == "-Command" {
        cmd.args(["-NoProfile", "-NonInteractive"]);
        utf8::powershell_command(command)
    } else if flag == "/C" && cfg!(windows) {
        utf8::cmd_command(command)
    } else {
        command.to_string()
    };
    let command = if !cfg!(windows) {
        match background_shell::session_environment()? {
            Some(script)
                if matches!(shell_type.to_lowercase().as_str(), "bash" | "sh" | "zsh") =>
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
    let command = match &cwd_state {
        // A command that exits early keeps the previous directory.
        Some(state) if powershell => format!(
            "{command}\n$__dcl_ok = $?; $__dcl_code = $global:LASTEXITCODE\ntry {{ [System.IO.File]::WriteAllText('{}', (Get-Location).ProviderPath) }} catch {{}}\nif (-not $__dcl_ok) {{ if ($__dcl_code) {{ exit $__dcl_code }} else {{ exit 1 }} }}",
            state.to_string_lossy().replace('\'', "''")
        ),
        Some(state) if cmd_shell => cmd_cwd_footer(&command, &state.to_string_lossy()),
        // Git Bash on Windows prints C:/... with -W; elsewhere -P is the path.
        Some(state) => format!(
            "{command}\n__dcl_status=$?\n{} > '{}' 2>/dev/null\nexit $__dcl_status",
            if cfg!(windows) { "{ pwd -W 2>/dev/null || pwd -P; }" } else { "pwd -P" },
            state.to_string_lossy().replace('\'', "'\"'\"'")
        ),
        None => command,
    };
    #[cfg(windows)]
    if flag == "/C" {
        // cmd uses its own quoting rules; normal Windows argument escaping adds
        // literal backslashes before quotes in nested PowerShell file paths.
        cmd.args(["/D", "/S", "/C"]).raw_arg(format!("\"{command}\""));
    } else {
        cmd.arg(&flag).arg(&command);
    }
    #[cfg(not(windows))]
    cmd.arg(&flag).arg(&command);
    cmd.current_dir(&safe_cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Background commands are named by their description, or the command.
    let description = input["description"].as_str().filter(|text| !text.trim().is_empty()).unwrap_or(&command).to_owned();
    if background {
        background_shell::start(cmd, input.get("timeout").filter(|value| !value.is_null()).map(|_| timeout), shell_type, &description, &command)
    } else {
        // Ctrl+B can move it to the background while it runs.
        let filtered = input.get("__sandbox").filter(|policy| policy["network_rules"].is_object() && input["dangerouslyDisableSandbox"] != true);
        let mark = filtered.map(sandbox::blocked_mark);
        let mut result = background_shell::run_foreground(cmd, timeout, shell_type, &description, &command).await?;
        // Name refused hosts: an HTTPS client often reports only a reset.
        if let (Some(policy), Some(mark)) = (filtered, mark) {
            let hosts = sandbox::blocked_since(policy, mark);
            if let (false, Some(object)) = (hosts.is_empty(), result.as_object_mut()) {
                object.insert("sandbox_network".into(), serde_json::json!(format!("The sandbox blocked network access to {}: not in sandbox.network.allowedDomains (or listed in deniedDomains) in settings.json. Ask the user to allow the domain; do not retry with other hosts.", hosts.join(", "))));
            }
        }
        if let Some(state) = &cwd_state {
            let moved = saved_cwd(state);
            if moved.as_deref().is_some_and(|dir| !is_path_allowed(Path::new(dir), cwd)) {
                let _ = fs::remove_file(state);
                if let Some(object) = result.as_object_mut() {
                    object.insert("note".into(), serde_json::json!(format!("Shell cwd was reset to {cwd}")));
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

pub fn find_shell(shell_name: &str) -> Option<String> {
    let candidates = match (shell_name, cfg!(target_os = "windows")) {
        ("bash", true) => vec![
            r"C:\Program Files\Git\bin\bash.exe".to_string(),
            r"C:\Program Files (x86)\Git\bin\bash.exe".to_string(),
            r"C:\Git\bin\bash.exe".to_string(),
        ],
        ("bash", false) => vec![
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
    let dir_path = input["path"].as_str().ok_or_else(|| anyhow!("path required"))?;

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
    let list = |key: &str| -> Vec<String> { input[key].as_array().into_iter().flatten().filter_map(|d| d.as_str()).map(|d| d.trim().trim_start_matches("*.").trim_start_matches('.').to_ascii_lowercase()).filter(|d| !d.is_empty()).collect() };
    let (allowed, blocked) = (list("allowed_domains"), list("blocked_domains"));
    if allowed.is_empty() && blocked.is_empty() { return; }
    let matches = |host: &str, domain: &str| host == domain || host.ends_with(&format!(".{domain}"));
    if let Some(results) = result.get_mut("results").and_then(|r| r.as_array_mut()) {
        results.retain(|item| {
            let Some(host) = item["url"].as_str().and_then(|url| reqwest::Url::parse(url).ok()).and_then(|url| url.host_str().map(str::to_ascii_lowercase)) else { return allowed.is_empty() };
            (allowed.is_empty() || allowed.iter().any(|d| matches(&host, d))) && !blocked.iter().any(|d| matches(&host, d))
        });
    }
}

/// The model's WebFetch: the URL policy applies before any request is made.
fn model_fetch_input(mut input: serde_json::Value) -> Result<serde_json::Value> {
    let url = input["url"].as_str().ok_or_else(|| anyhow!("url required"))?;
    input["url"] = serde_json::json!(web::model_url(url, cfg!(test))?);
    Ok(input)
}
fn tool_web_fetch_blocking(input: serde_json::Value) -> Result<serde_json::Value> { web::fetch_blocking(model_fetch_input(input)?) }
async fn tool_web_fetch_async(input: serde_json::Value) -> Result<serde_json::Value> { web::fetch(model_fetch_input(input)?).await }
fn tool_web_search_blocking(_input: serde_json::Value) -> Result<serde_json::Value> {
    Err(anyhow!("WebSearch requires a conversation provider and its search settings. Start search from a conversation."))
}

pub fn run_git_command(args: &[&str], cwd: &str) -> Result<std::process::Output> {
    let mut cmd = std::process::Command::new("git");
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd.args(args)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| anyhow!("Failed to execute git: {}", e))
}

fn tool_git_status(input: serde_json::Value) -> Result<serde_json::Value> {
    let path = input["path"].as_str().ok_or_else(|| anyhow!("path required"))?;

    let output = run_git_command(&["status", "--porcelain"], path)?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if !output.status.success() && !stderr.is_empty() {
        return Ok(serde_json::json!({
            "error": stderr.trim(),
            "is_error": true
        }));
    }

    let files: Vec<serde_json::Value> = stdout.lines().filter(|l| !l.is_empty()).map(|line| {
        let status = &line[..2];
        let file_path = &line[3..];
        serde_json::json!({
            "status": status.trim(),
            "file": file_path
        })
    }).collect();

    Ok(serde_json::json!({
        "files": files,
        "count": files.len(),
        "raw": stdout
    }))
}

fn tool_git_diff(input: serde_json::Value) -> Result<serde_json::Value> {
    let path = input["path"].as_str().ok_or_else(|| anyhow!("path required"))?;
    let staged = input["staged"].as_bool().unwrap_or(false);
    let file = input["file"].as_str();

    let mut args: Vec<&str> = vec!["diff"];
    if staged {
        args.push("--staged");
    }
    if let Some(f) = file {
        args.push("--");
        args.push(f);
    }

    let output = run_git_command(&args, path)?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if !output.status.success() && !stderr.is_empty() {
        return Ok(serde_json::json!({
            "error": stderr.trim(),
            "is_error": true
        }));
    }

    Ok(serde_json::json!({
        "diff": stdout,
        "staged": staged,
        "file": file
    }))
}

fn tool_git_log(input: serde_json::Value) -> Result<serde_json::Value> {
    let path = input["path"].as_str().ok_or_else(|| anyhow!("path required"))?;
    let count = input["count"].as_u64().unwrap_or(10);
    let oneline = input["oneline"].as_bool().unwrap_or(true);

    let mut args: Vec<String> = vec!["log".to_string()];
    if oneline {
        args.push("--oneline".to_string());
    }
    args.push(format!("-n{}", count));
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let output = run_git_command(&arg_refs, path)?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if !output.status.success() && !stderr.is_empty() {
        return Ok(serde_json::json!({
            "error": stderr.trim(),
            "is_error": true
        }));
    }

    let commits: Vec<serde_json::Value> = stdout.lines().filter(|l| !l.is_empty()).map(|line| {
        if oneline {
            let parts: Vec<&str> = line.splitn(2, ' ').collect();
            serde_json::json!({
                "hash": parts.first().unwrap_or(&""),
                "message": parts.get(1).unwrap_or(&"")
            })
        } else {
            serde_json::json!({ "raw": line })
        }
    }).collect();

    Ok(serde_json::json!({
        "commits": commits,
        "count": commits.len(),
        "raw": stdout
    }))
}

fn tool_git_commit(input: serde_json::Value) -> Result<serde_json::Value> {
    let path = input["path"].as_str().ok_or_else(|| anyhow!("path required"))?;
    let message = input["message"].as_str().ok_or_else(|| anyhow!("message required"))?;

    let add_output = run_git_command(&["add", "-A"], path)?;
    if !add_output.status.success() {
        let stderr = String::from_utf8_lossy(&add_output.stderr).to_string();
        if !stderr.is_empty() {
            return Ok(serde_json::json!({
                "error": format!("git add failed: {}", stderr.trim()),
                "is_error": true
            }));
        }
    }

    let commit_output = run_git_command(&["commit", "-m", message], path)?;

    let stdout = String::from_utf8_lossy(&commit_output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&commit_output.stderr).to_string();

    if !commit_output.status.success() {
        return Ok(serde_json::json!({
            "error": format!("git commit failed: {}", if stderr.is_empty() { &stdout } else { &stderr }.trim()),
            "is_error": true
        }));
    }

    Ok(serde_json::json!({
        "success": true,
        "output": stdout,
        "message": message
    }))
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

fn tool_git_add(input: serde_json::Value) -> Result<serde_json::Value> {
    let path = input["path"].as_str().ok_or_else(|| anyhow!("path required"))?;
    let files = input["files"].as_array().ok_or_else(|| anyhow!("files array required"))?;

    let file_strs: Vec<String> = files.iter()
        .filter_map(|f| f.as_str().map(String::from))
        .collect();

    if file_strs.is_empty() {
        return Ok(serde_json::json!({
            "error": "No files specified",
            "is_error": true
        }));
    }

    let mut args: Vec<String> = vec!["add".to_string()];
    args.extend(file_strs.clone());
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let output = run_git_command(&arg_refs, path)?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if !output.status.success() && !stderr.is_empty() {
        return Ok(serde_json::json!({
            "error": stderr.trim(),
            "is_error": true
        }));
    }

    Ok(serde_json::json!({
        "success": true,
        "files": file_strs,
        "output": stdout
    }))
}

async fn tool_web_search_async(input: serde_json::Value) -> Result<serde_json::Value> { tool_web_search_blocking(input) }

pub(crate) async fn execute_public_web_search(input: serde_json::Value) -> Result<serde_json::Value> { web::search(input).await }
