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

mod lsp;
pub mod macros;
mod notebook;
pub mod plan_import;
mod read;
mod search;
pub mod tasks;

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
pub mod background_shell;
mod bash_process;
pub mod monitor;
pub mod file_change;
pub(crate) mod image_generation;
pub(crate) mod path_scope;
pub(crate) mod provider_search;
mod shell_discovery;
mod web;
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
        image_generation::definition(),
        ToolDefinition { name: "Skill".into(), description: "Invoke a skill.\n\nA skill is a packaged set of instructions the user or project has set up for a particular kind of task (deploy steps, a review checklist, a repo-specific workflow). Available skills are listed in the system prompt with one-line descriptions. When the task at hand is one a listed skill covers, call this tool first — the skill's instructions load into the turn for you to follow in place of your default approach; skills with context:fork instead run in an isolated child conversation and return the finished result. Users may also ask for one by name (`/<name>`, or \"slash command\"); that's a request to invoke it.\n\n- `skill`: exact name from the listing, no leading slash. Plugin skills use `plugin:skill`.\n- `args`: optional arguments to pass through.\n\nOnly names from the listing (or that the user typed explicitly) are valid. Built-in CLI commands (`/help`, `/clear`, …) aren't skills. Do not read a skill file to bypass this tool: dynamic shell context and fork isolation run through it.".into(), input_schema:serde_json::json!({"type":"object","properties":{"skill":{"description":"The name of a skill from the available-skills list. Do not guess names.","type":"string"},"args":{"description":"Optional arguments for the skill","type":"string"}},"required":["skill"],"additionalProperties":false}) },
        notebook::definition(),
        lsp::definition(),
        // `worktree` stays dispatchable for older transcripts; the model sees Enter/ExitWorktree.
        agent_messages::definition(),
        agent_messages::list_agents_definition(),
        // `cron` stays dispatchable for older transcripts; the model sees the reference trio.
        crate::services::goal_proposals::definition(),
        ToolDefinition { name: "ToolSearch".into(), description: "Load built-in tools that are not in this request yet. A successful search returns their real definitions with schemas and marks them loaded, so they can be called from the next model request on. Select complete tool names separated by commas or whitespace, e.g. `WebSearch WebFetch` or `WebSearch,WebFetch`; `select:Name1,Name2` (also accepting whitespace) explicitly selects exact names. Otherwise every keyword must appear in a tool name or description. Tools that commonly load on demand include listing directories (ListDir), multiple edits (MultiEdit), web access (WebFetch, WebSearch), Git and LSP helpers, subagents (Agent, SendMessage), Workflow, long-term memory, scheduled jobs and desktop control. Searching costs nothing and grants no permission: the tool still goes through the usual approval and scope checks. MCP tools listed as available on demand load the same way.".into(), input_schema: serde_json::json!({"type":"object","properties":{"query":{"type":"string","description":"Complete tool names separated by commas or whitespace, or keywords describing what you need"}},"required":["query"]}) },
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
            description: "Performs exact string replacement in a file.\n\n- You must Read the file in this conversation before editing, or the call will fail.\n- `old_string` must match the file including indentation and be unique (LF and CRLF are equivalent) — the edit fails otherwise. Strip the Read line prefix (line number + tab) before matching.\n- `replace_all: true` replaces every occurrence instead.".to_string(),
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
            description: "Executes a shell command and returns its output.\n\nThe shell defaults to shell.toolDefault in settings.json (bash / Git Bash on Windows when unset): use Unix shell syntax there — `/dev/null` not `NUL`, forward slashes, `$VAR` not `%VAR%` or `$env:VAR`. Pass `shell` (powershell, pwsh, cmd, sh, zsh, fish, wsl) to use another shell's syntax; PowerShell input/output is UTF-8.\n\n- With bash, sh or zsh the working directory persists between calls, but prefer absolute paths; leaving the project resets it to the project directory. Shell state (env vars, functions) does not persist. Other shells start in the project directory each call.\n- Command output is displayed to you, not reliably to the user.\n- `timeout` is in milliseconds: default 120000, max 600000 for a foreground command.\n- `run_in_background` runs the command as a managed task: it keeps running across turns and returns a task_id. No `&` or Start-Process needed. You will be notified when it finishes, so do not poll or sleep waiting for it. To read its output before then, or to wait for it in a non-interactive run (nothing wakes the session after such a turn ends), load TaskGet with ToolSearch and call it with the task_id (block:true waits up to timeout milliseconds). TaskStop stops it.
- When a call finishes it cleans up the command's whole process tree, including children the command started. A service that must outlive the call (a local server, a daemon) needs `detach: true`, which starts it outside that tree and returns its pid and log files.
- With bash and zsh a pipeline reports an earlier command's failure, so `failing | head` is not reported as success. `pipeline_sigpipe: true` means the output was only cut short by a reader such as `head`.\n\n# Git\n- Interactive flags (`-i`, e.g. `git rebase -i`, `git add -i`) are not supported in this environment.\n- Use the `gh` CLI for GitHub operations (PRs, issues, API).\n- Commit or push only when the user asks. If on the default branch, branch first.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "description": "The command to execute", "type": "string" },
                    "timeout": { "description": "Positive timeout in milliseconds: 1..600000 foreground, 1..86400000 background. Omit for no background runtime limit; zero is invalid.", "type": "number", "minimum": 1, "maximum": 86400000 },
                    "description": { "description": "Clear, concise description of what this command does in active voice. Never use words like \"complex\" or \"risk\" in the description - just describe what it does.\n\nSay what the command does in plain words: do not echo the command's text, its flags, or file paths - the user reads this description, often without seeing the command.\n\nFor simple commands (git, npm, standard CLI tools), keep it brief (5-10 words):\n- ls → \"List files in current directory\"\n- git status → \"Show working tree status\"\n- npm install → \"Install package dependencies\"\n\nFor commands that are harder to parse at a glance (piped commands, obscure flags, etc.), add enough context to clarify what it does:\n- find . -name \"*.tmp\" -exec rm {} \\; → \"Find and delete all .tmp files recursively\"\n- git reset --hard origin/main → \"Discard all local changes and match remote main\"\n- curl -s url | jq '.data[]' → \"Fetch JSON from URL and extract data array elements\"", "type": "string" },
                    "run_in_background": { "description": "Set to true to run this command in the background.", "type": "boolean" },
                    "detach": { "description": "Start an unmanaged service that must outlive this call, such as a local server or daemon. The command is not waited for and is not added to the process tree that a normal call cleans up, so it keeps running after this call, using `detach` with `run_in_background` or `timeout` is an error. Use it only when the process must survive; otherwise run the command normally or use `run_in_background`.", "type": "boolean" },
                    "dangerouslyDisableSandbox": { "description": "Set this to true to dangerously override sandbox mode and run commands without sandboxing.", "type": "boolean" },
                    "shell": { "description": "Shell to use: bash, zsh, fish, sh, cmd, powershell, pwsh, wsl (explicit configured WSL distribution; default: shell.toolDefault, otherwise bash)", "type": "string" }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        },
        ToolDefinition {
            name: "Monitor".to_string(),
            description: "Runs a shell script in the background for this session and streams its output to you: every stdout line becomes a notification that wakes you, so you never poll. Lines within 200 ms arrive together. Use it to watch logs, builds, CI or any condition over time: write a script that prints a line only when something you would act on happens (for example `tail -f app.log | grep --line-buffered ERROR`, or a polling loop that echoes on change). Each pipe stage must flush per line. stderr is logged, not notified. The monitor stops after timeout_ms (default 1800000 = 30 min, max 3600000); you are notified when it exits, expires or is stopped, and can start it again. More than 30 events a minute stops it, so filter tightly. It runs only while this session runs. TaskStop with its task_id stops it. For a single 'tell me when X finishes', use Bash run_in_background instead.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "description": "The shell script to run in the background", "type": "string" },
                    "description": { "description": "Short description shown in notifications", "type": "string" },
                    "timeout_ms": { "description": "How long the monitor runs, in milliseconds", "type": "integer", "minimum": 1000, "maximum": 3600000, "default": 1800000 },
                    "shell": { "description": "Shell to use: bash, zsh, fish, sh, cmd, powershell, pwsh, wsl (explicit configured WSL distribution; default: shell.toolDefault, otherwise bash)", "type": "string" }
                },
                "required": ["command", "description"],
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
    for def in &mut defs {
        if matches!(def.name.as_str(), "ListDir" | "git_status" | "git_diff") {
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
            _ if $name.is_empty() => Ok(serde_json::json!({
                "error": "Tool call has no name; the model returned an incomplete tool call"
            })),
            _ => Ok(serde_json::json!({ "error": format!("Unknown tool: {}", $name) })),
        }
    };
}

fn directory_input(
    name: &str,
    mut input: serde_json::Value,
    cwd: &str,
) -> Result<serde_json::Value> {
    if matches!(name, "ListDir" | "git_status" | "git_diff") {
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
    if name == "computer_use" {
        return tool_computer_use_async(input).await;
    }
    if name == "LSP" || name == "lsp" {
        return lsp::execute(input, cwd).await;
    }
    if name == "worktree" {
        return worktrees::execute(input, cwd).await;
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

/// The host-level fallback: it only lists definitions and has no session, so it
/// cannot load anything. `success` stays false so a stored result can never be
/// read back as a load.
fn tool_search(input: serde_json::Value) -> Result<serde_json::Value> {
    let query = input["query"]
        .as_str()
        .ok_or_else(|| anyhow!("query is required"))?
        .to_lowercase();
    let tools: Vec<_> = get_tool_definitions()
        .into_iter()
        .filter(|tool| {
            tool.name.to_lowercase().contains(&query)
                || tool.description.to_lowercase().contains(&query)
        })
        .collect();
    Ok(serde_json::json!({
        "success": false,
        "loaded_tools": [],
        "tools": tools,
        "content": "This host entry only lists definitions; a session loads tools through its own engine.",
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
    assert_eq!(
        out["stdout"].as_str().unwrap().trim(),
        project.join("sub").to_str().unwrap()
    );
    // Exiting early keeps the previous directory.
    let _ = tool_bash_async(run("cd /; exit 3"), cwd).await;
    assert!(tool_bash_async(run("pwd -P"), cwd).await.unwrap()["stdout"]
        .as_str()
        .unwrap()
        .contains("sub"));
    let reset = tool_bash_async(run("cd /"), cwd).await.unwrap();
    assert_eq!(reset["note"], format!("Shell cwd was reset to {cwd}"));
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
    let timeout_ms = match input.get("timeout_ms").filter(|value| !value.is_null()) {
        Some(value) => value
            .as_u64()
            .filter(|ms| (1_000..=3_600_000).contains(ms))
            .ok_or_else(|| anyhow!("Monitor timeout_ms must be between 1000 and 3600000"))?,
        None => 1_800_000,
    };
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
    let safe_cwd = match cwd_state.as_ref().and_then(saved_cwd) {
        Some(saved) if Path::new(&saved).is_dir() && is_path_allowed(Path::new(&saved), cwd) => {
            saved
        }
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
    let command = match &cwd_state {
        // A command that exits early keeps the previous directory.
        Some(state) if resolved.wsl.is_some() => {
            let path = crate::services::wsl::linux_path(resolved.wsl.as_ref().unwrap(), state).await?;
            format!("{command}\n__dcl_status=$?\nwslpath -w \"$PWD\" > '{}' 2>/dev/null\nexit $__dcl_status", path.replace('\'', "'\"'\"'"))
        }
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
    if detach {
        let directory = crate::runtime::resolve_default_data_dir()?
            .join("detached-shells")
            .join(uuid::Uuid::new_v4().to_string());
        let mut result = bash_process::start_detached(cmd, &directory, actual_shell)?;
        std::fs::write(directory.join("command.txt"), command.as_bytes())?;
        result["directory"] = serde_json::json!(directory);
        result["shell_path"] = serde_json::json!(shell);
        Ok(result)
    } else if background {
        if let Some(timeout_ms) = input
            .get("__monitor_timeout_ms")
            .and_then(serde_json::Value::as_u64)
        {
            let mut result = monitor::start(
                cmd,
                &description,
                &command,
                std::time::Duration::from_millis(timeout_ms),
                wsl_guard,
            )?;
            result["shell_path"] = serde_json::json!(shell);
            return Ok(result);
        }
        let mut result = background_shell::start(
            cmd,
            input
                .get("timeout")
                .filter(|value| !value.is_null())
                .map(|_| timeout),
            actual_shell,
            &description,
            &command,
            wsl_guard,
        )?;
        result["shell_path"] = serde_json::json!(shell);
        Ok(result)
    } else {
        // Ctrl+B can move it to the background while it runs.
        let filtered = input.get("__sandbox").filter(|policy| {
            policy["network_rules"].is_object() && input["dangerouslyDisableSandbox"] != true
        });
        let mark = filtered.map(sandbox::blocked_mark);
        let mut result = background_shell::run_foreground(
            cmd,
            timeout,
            actual_shell,
            &description,
            &command,
            wsl_guard,
        )
        .await?;
        result["shell_path"] = serde_json::json!(shell);
        // Name refused hosts: an HTTPS client often reports only a reset.
        if let (Some(policy), Some(mark)) = (filtered, mark) {
            let hosts = sandbox::blocked_since(policy, mark);
            if let (false, Some(object)) = (hosts.is_empty(), result.as_object_mut()) {
                object.insert("sandbox_network".into(), serde_json::json!(format!("The sandbox blocked network access to {}: not in sandbox.network.allowedDomains (or listed in deniedDomains) in settings.json. Ask the user to allow the domain; do not retry with other hosts.", hosts.join(", "))));
            }
        }
        if let Some(state) = &cwd_state {
            let moved = saved_cwd(state);
            if moved
                .as_deref()
                .is_some_and(|dir| !is_path_allowed(Path::new(dir), cwd))
            {
                let _ = fs::remove_file(state);
                if let Some(object) = result.as_object_mut() {
                    object.insert(
                        "note".into(),
                        serde_json::json!(format!("Shell cwd was reset to {cwd}")),
                    );
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
    input["url"] = serde_json::json!(web::model_url(
        url,
        cfg!(test) || path_scope::approved_local_fetch(url)
    )?);
    Ok(input)
}
fn tool_web_fetch_blocking(input: serde_json::Value) -> Result<serde_json::Value> {
    web::fetch_blocking(model_fetch_input(input)?)
}
async fn tool_web_fetch_async(input: serde_json::Value) -> Result<serde_json::Value> {
    web::fetch(model_fetch_input(input)?).await
}
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
    let path = input["path"]
        .as_str()
        .ok_or_else(|| anyhow!("path required"))?;

    let output = run_git_command(&["status", "--porcelain"], path)?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if !output.status.success() && !stderr.is_empty() {
        return Ok(serde_json::json!({
            "error": stderr.trim(),
            "is_error": true
        }));
    }

    let files: Vec<serde_json::Value> = stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(|line| {
        let status = &line[..2];
        let file_path = &line[3..];
        serde_json::json!({
            "status": status.trim(),
            "file": file_path
        })
        })
        .collect();

    Ok(serde_json::json!({
        "files": files,
        "count": files.len(),
        "raw": stdout
    }))
}

fn tool_git_diff(input: serde_json::Value) -> Result<serde_json::Value> {
    let path = input["path"]
        .as_str()
        .ok_or_else(|| anyhow!("path required"))?;
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
    let path = input["path"]
        .as_str()
        .ok_or_else(|| anyhow!("path required"))?;
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

    let commits: Vec<serde_json::Value> = stdout
        .lines()
        .filter(|l| !l.is_empty())
        .map(|line| {
        if oneline {
            let parts: Vec<&str> = line.splitn(2, ' ').collect();
            serde_json::json!({
                "hash": parts.first().unwrap_or(&""),
                "message": parts.get(1).unwrap_or(&"")
            })
        } else {
            serde_json::json!({ "raw": line })
        }
        })
        .collect();

    Ok(serde_json::json!({
        "commits": commits,
        "count": commits.len(),
        "raw": stdout
    }))
}

fn tool_git_commit(input: serde_json::Value) -> Result<serde_json::Value> {
    let path = input["path"]
        .as_str()
        .ok_or_else(|| anyhow!("path required"))?;
    let message = input["message"]
        .as_str()
        .ok_or_else(|| anyhow!("message required"))?;

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
    let path = input["path"]
        .as_str()
        .ok_or_else(|| anyhow!("path required"))?;
    let files = input["files"]
        .as_array()
        .ok_or_else(|| anyhow!("files array required"))?;
    anyhow::ensure!(
        files.iter().all(serde_json::Value::is_string),
        "Every requested Git path must be a string"
    );

    let file_strs: Vec<String> = files
        .iter()
        .filter_map(|f| f.as_str().map(String::from))
        .collect();

    if file_strs.is_empty() {
        return Ok(serde_json::json!({
            "error": "No files specified",
            "is_error": true
        }));
    }

    let mut args: Vec<String> = vec!["add".to_string(), "--".to_string()];
    args.extend(file_strs.clone());
    let arg_refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();

    let output = run_git_command(&arg_refs, path)?;

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    if !output.status.success() {
        return Ok(serde_json::json!({
            "error": if !stderr.trim().is_empty() { stderr.trim() } else if !stdout.trim().is_empty() { stdout.trim() } else { "git add failed without output" },
            "success": false,
            "exit_code": output.status.code().unwrap_or(-1),
            "is_error": true
        }));
    }

    Ok(serde_json::json!({
        "success": true,
        "files": file_strs,
        "output": stdout
    }))
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
    fn git_add_reports_requested_paths_and_real_failures() {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().to_str().unwrap();
        assert!(run_git_command(&["init"], cwd).unwrap().status.success());
        fs::write(root.path().join("--force"), "requested file").unwrap();
        fs::write(root.path().join("untouched.txt"), "keep unstaged").unwrap();
        let result = tool_git_add(json!({"path":cwd,"files":["--force"]})).unwrap();
        assert_eq!(result["success"], true, "{result}");
        assert_eq!(result["files"], json!(["--force"]));
        let staged = run_git_command(&["diff", "--cached", "--name-only"], cwd).unwrap();
        assert_eq!(String::from_utf8_lossy(&staged.stdout).trim(), "--force");
        let failed = tool_git_add(json!({"path":cwd,"files":["missing.txt"]})).unwrap();
        assert_eq!(failed["success"], false);
        assert_ne!(failed["exit_code"], 0);
        assert!(tool_git_add(json!({"path":cwd,"files":[7]})).is_err());
    }

    #[tokio::test]
    async fn omitted_directory_uses_session_workspace() {
        let root = tempfile::tempdir().unwrap();
        fs::write(root.path().join("sentinel.txt"), "hello").unwrap();
        let cwd = root.path().to_str().unwrap();
        assert!(run_git_command(&["init"], cwd).unwrap().status.success());
        for input in [json!({}), json!({"path":"."}), json!({"path":null})] {
            let list = execute_tool("ListDir", input.clone(), cwd).unwrap();
            assert!(list["entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["name"] == "sentinel.txt"));
            let status = execute_tool_async("git_status", input.clone(), cwd)
                .await
                .unwrap();
            assert!(status["files"]
                .as_array()
                .unwrap()
                .iter()
                .any(|file| file["file"] == "sentinel.txt"));
            assert_eq!(execute_tool("git_diff", input, cwd).unwrap()["diff"], "");
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
