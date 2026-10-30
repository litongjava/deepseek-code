//! Tips shown under the activity line while a turn runs. Each tip has a
//! cooldown in sessions; the eligible tip shown longest ago is chosen, and the
//! history lives in `<data dir>/tips.json`. `"spinnerTipsEnabled": false` in
//! `<data dir>/settings.json` turns them off.
use serde_json::{json, Value};
use std::path::Path;

struct Tip {
    id: &'static str,
    text: &'static str,
    cooldown: u64,
    relevant: fn() -> bool,
}

fn always() -> bool {
    true
}

fn windows() -> bool {
    cfg!(windows)
}

fn missing_language_server() -> bool {
    let workspace = std::env::current_dir().unwrap_or_default();
    deepseek_code_core::tools::lsp_servers::LANGUAGES
        .iter()
        .any(|language| deepseek_code_core::tools::lsp_servers::locate(language, &workspace).is_none())
}

const TIPS: &[Tip] = &[
    Tip { id: "btw", text: "Use /btw to ask a quick side question without interrupting the current task", cooldown: 3, relevant: always },
    Tip { id: "plan", text: "Use /plan for larger changes: the model investigates read-only and asks before editing", cooldown: 5, relevant: always },
    Tip { id: "ctrl-o", text: "Press Ctrl+O after a turn to expand every tool call with its full output", cooldown: 4, relevant: always },
    Tip { id: "shift-tab", text: "Shift+Tab cycles permission modes: ask, accept edits, plan and more", cooldown: 6, relevant: always },
    Tip { id: "queue", text: "Type while the model works: your prompt runs as soon as this turn ends", cooldown: 6, relevant: always },
    Tip { id: "esc", text: "Esc interrupts the current turn; the conversation so far is kept", cooldown: 10, relevant: always },
    Tip { id: "lsp", text: "Run /lsp to see language servers; /lsp install java adds code navigation for Java", cooldown: 8, relevant: missing_language_server },
    Tip { id: "compact", text: "Use /compact when a long conversation slows down; the full history stays archived", cooldown: 10, relevant: always },
    Tip { id: "continue", text: "Run dcl --continue to resume the latest session in this directory", cooldown: 10, relevant: always },
    Tip { id: "provider", text: "/provider add saves a model provider; /model switches between its models", cooldown: 12, relevant: always },
    Tip { id: "test", text: "/test sends a tiny request to check the current provider and model", cooldown: 15, relevant: always },
    Tip { id: "rename", text: "Use /rename to give this session a name you can find again", cooldown: 15, relevant: always },
    Tip { id: "ctrl-r", text: "Ctrl+R searches your earlier prompts", cooldown: 12, relevant: always },
    Tip { id: "newline", text: "Alt+Enter adds a new line to your prompt", cooldown: 12, relevant: always },
    Tip { id: "background", text: "/background detaches this task so you can keep using the terminal", cooldown: 15, relevant: always },
    Tip { id: "max-tokens", text: "If long files get cut off, start dcl with a higher --max-tokens", cooldown: 20, relevant: always },
    Tip { id: "json-schema", text: "dcl -p --json-schema returns validated JSON, handy for scripts", cooldown: 20, relevant: always },
    Tip { id: "remote", text: "/remote-control continues this session from another device", cooldown: 25, relevant: always },
    Tip { id: "team-sync", text: "dcl team-sync shares team memories through a self-hosted server", cooldown: 25, relevant: always },
    Tip { id: "status", text: "/status shows the session, model, permission mode and servers in use", cooldown: 15, relevant: always },
    Tip { id: "windows-shell", text: "On Windows the Bash tool runs PowerShell; ask for PowerShell syntax when it matters", cooldown: 15, relevant: windows },
];

fn enabled(data_dir: &Path) -> bool {
    std::fs::read_to_string(data_dir.join("settings.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .is_none_or(|settings| settings["spinnerTipsEnabled"] != Value::Bool(false))
}

fn load(data_dir: &Path) -> Value {
    std::fs::read_to_string(data_dir.join("tips.json"))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({"sessions": 0, "shown": {}}))
}

fn save(data_dir: &Path, history: &Value) {
    let _ = std::fs::write(data_dir.join("tips.json"), history.to_string());
}

/// Counts an interactive session; cooldowns are measured in sessions.
pub fn start_session(data_dir: &Path) {
    let mut history = load(data_dir);
    history["sessions"] = json!(history["sessions"].as_u64().unwrap_or(0) + 1);
    save(data_dir, &history);
}

fn choose(history: &Value, relevant: impl Fn(&Tip) -> bool) -> Option<&'static Tip> {
    let session = history["sessions"].as_u64().unwrap_or(0);
    TIPS.iter()
        .filter_map(|tip| {
            let since = match history["shown"][tip.id].as_u64() {
                Some(shown) => session.saturating_sub(shown),
                None => u64::MAX,
            };
            (since >= tip.cooldown).then_some((tip, since))
        })
        .filter(|(tip, _)| relevant(tip))
        .max_by_key(|(_, since)| *since)
        .map(|(tip, _)| tip)
}

/// The tip for the next turn, recorded as shown. A shown tip waits for its
/// cooldown, so each turn of a session gets a different one until none is left.
pub fn next(data_dir: &Path) -> Option<String> {
    if !enabled(data_dir) {
        return None;
    }
    let mut history = load(data_dir);
    let session = history["sessions"].as_u64().unwrap_or(0);
    let tip = choose(&history, |tip| (tip.relevant)())?;
    history["shown"][tip.id] = json!(session);
    save(data_dir, &history);
    Some(tip.text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tips_rotate_by_cooldown_and_can_be_disabled() {
        let dir = tempfile::tempdir().unwrap();
        start_session(dir.path());
        let first = next(dir.path()).unwrap();
        let second = next(dir.path()).unwrap();
        assert_ne!(first, second, "a shown tip waits for its cooldown");

        let history = json!({"sessions": 100, "shown": {"btw": 99}});
        let chosen = choose(&history, |_| true).unwrap();
        assert_ne!(chosen.id, "btw");
        assert!(choose(&history, |tip| tip.id == "btw").is_none());

        std::fs::write(dir.path().join("settings.json"), r#"{"spinnerTipsEnabled":false}"#).unwrap();
        start_session(dir.path());
        assert!(next(dir.path()).is_none());
    }
}
