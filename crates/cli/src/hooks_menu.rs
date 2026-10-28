//! `/hooks` as in the reference CLI: a read-only menu of hook events, each
//! with its description and the hooks configured for it.
use crate::terminal::picker::{Entry, Picker};
use anyhow::Result;
use claude_core::services::tool_hooks::ActiveHook;

/// Every event dcl runs hooks for, in the reference CLI's wording.
const EVENTS: &[(&str, &str)] = &[
    ("PreToolUse", "Before tool execution"),
    ("PostToolUse", "After tool execution"),
    ("PostToolUseFailure", "After tool execution fails"),
    ("PermissionRequest", "When a permission prompt is shown"),
    ("PermissionDenied", "After auto mode classifier denies a tool call"),
    ("Notification", "When notifications are sent"),
    ("UserPromptSubmit", "When the user submits a prompt"),
    ("SessionStart", "When a new session is started"),
    ("SessionEnd", "When a session is ending"),
    ("Stop", "Right before DeepSeek Code concludes its response"),
    ("StopFailure", "When the turn ends due to an API error"),
    ("SubagentStart", "When a subagent starts"),
    ("SubagentStop", "Right before a subagent concludes its response"),
    ("PreCompact", "Before conversation compaction"),
    ("PostCompact", "After conversation compaction"),
    ("Setup", "When the repository is set up"),
    ("ConfigChange", "When settings change"),
    ("InstructionsLoaded", "When project instructions are loaded"),
    ("CwdChanged", "When the working directory changes"),
    ("FileChanged", "When a watched file changes"),
    ("WorktreeCreate", "When a worktree is created"),
    ("WorktreeRemove", "When a worktree is removed"),
    ("Elicitation", "When an MCP server asks for input"),
    ("ElicitationResult", "After the user answers an MCP server"),
    ("TeammateIdle", "When a teammate becomes idle"),
    ("TaskCreated", "When a task is created"),
    ("TaskCompleted", "When a task is completed"),
];

fn event_name(hook: &ActiveHook) -> String {
    serde_json::to_value(hook.event).ok().and_then(|value| value.as_str().map(str::to_owned)).unwrap_or_default()
}

fn action(hook: &ActiveHook) -> String {
    if let Some(http) = serde_json::to_value(&hook.http).ok().filter(|value| !value.is_null()) {
        return format!("http {}", http["url"].as_str().unwrap_or_default());
    }
    if hook.prompt.is_some() {
        return "prompt".into();
    }
    hook.command.clone()
}

pub fn run(hooks: &[ActiveHook]) -> Result<()> {
    let total = if hooks.is_empty() {
        "no hooks yet".to_owned()
    } else {
        format!("{} hook{}", hooks.len(), if hooks.len() == 1 { "" } else { "s" })
    };
    let subtitle = format!("{total} \u{b7} This menu is read-only. To add or change a hook, edit settings.json or ask DeepSeek Code.");
    let top = [Entry { title: "All events".into(), detail: String::new() }];
    loop {
        let picker = Picker { heading: "Hooks", subtitle: Some(&subtitle), entries: &top, search: false, hint: "Enter to view \u{b7} Esc to close", selected: 0 };
        if picker.run()?.is_none() {
            return Ok(());
        }
        let mut selected = 0;
        loop {
            let entries: Vec<Entry> = EVENTS
                .iter()
                .map(|(name, description)| {
                    let count = hooks.iter().filter(|hook| event_name(hook) == *name).count();
                    let title = if count == 0 { name.to_string() } else { format!("{name} ({count})") };
                    Entry { title, detail: description.to_string() }
                })
                .collect();
            let events = Picker { heading: "Hook events", subtitle: None, entries: &entries, search: true, hint: "Type to search \u{b7} Enter to open \u{b7} Esc to go back", selected };
            let Some(index) = events.run()? else {
                break;
            };
            selected = index;
            let (name, description) = EVENTS[index];
            let configured: Vec<Entry> = hooks
                .iter()
                .filter(|hook| event_name(hook) == name)
                .map(|hook| Entry {
                    title: if hook.matcher.is_empty() { "(all)".into() } else { hook.matcher.clone() },
                    detail: format!("{} \u{b7} timeout {}s", action(hook), hook.timeout),
                })
                .collect();
            let empty = [Entry { title: "No hooks configured for this event".into(), detail: String::new() }];
            let heading = format!("{name} \u{b7} {description}");
            Picker {
                heading: &heading,
                subtitle: Some("Matcher, then what runs"),
                entries: if configured.is_empty() { &empty } else { &configured },
                search: false,
                hint: "Esc to go back",
                selected: 0,
            }
            .run()?;
        }
    }
}
