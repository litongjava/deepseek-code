//! Suggested next prompts. After a turn, a small tool-free request predicts
//! what the user is likely to type next; the editor shows it as placeholder
//! text that Tab accepts. `"promptSuggestionEnabled": false` in
//! `<data dir>/settings.json` turns this off.
use serde_json::Value;
use std::path::Path;

const SYSTEM: &str = "You predict the next message a developer will type into a coding assistant. Reply with only that message or with nothing.";

const PROMPT: &str = "[SUGGESTION MODE: Suggest what the user might naturally type next into DeepSeek Code.]

Look at the user's recent messages and original request. Predict what THEY would type, not what you think they should do. The test: would they think \"I was just about to type that\"?

Examples:
- User asked \"fix the bug and run tests\", bug is fixed -> \"run the tests\"
- After code was written -> \"try it out\"
- The assistant offered options -> the one the user would likely pick
- The assistant asked whether to continue -> \"yes\" or \"go ahead\"
- Task complete with an obvious follow-up -> \"commit this\" or \"push it\"
- After an error or misunderstanding -> nothing

Never suggest evaluations (\"looks good\", \"thanks\"), questions, assistant-voice text (\"Let me...\"), new ideas the user did not ask about, or several sentences. Stay silent if the next step is not obvious.

Format: 2-12 words in the user's language and style, or nothing. Reply with ONLY the suggestion, no quotes or explanation.";

pub fn enabled(data_dir: &Path) -> bool {
    std::fs::read_to_string(data_dir.join("settings.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .is_none_or(|settings| settings["promptSuggestionEnabled"] != Value::Bool(false))
}

/// The recent conversation as plain text, when it ends with an answer.
pub fn context(history: &[Value]) -> Option<String> {
    let last = history.last()?;
    if last["role"] != "assistant" || last["content"].as_str().is_none_or(|text| text.trim().is_empty()) {
        return None;
    }
    let start = history.len().saturating_sub(6);
    let mut text = String::from("Recent conversation:\n");
    for row in &history[start..] {
        let content = row["content"].as_str().unwrap_or_default();
        let clipped: String = content.chars().take(1500).collect();
        text.push_str(&format!("\n[{}]\n{clipped}\n", row["role"].as_str().unwrap_or("message")));
    }
    text.push('\n');
    text.push_str(PROMPT);
    Some(text)
}

/// Drops empty, meta, labelled or otherwise unusable suggestions.
pub fn filter(text: &str) -> Option<String> {
    let suggestion = text.trim().trim_matches(|c| c == '"' || c == '\'' || c == '`').trim();
    let lower = suggestion.to_lowercase();
    let words = suggestion.split_whitespace().count();
    let single_word_ok = suggestion.starts_with('/')
        || ["yes", "ok", "okay", "sure", "continue", "push", "commit", "deploy", "stop", "exit"].contains(&lower.as_str())
        || suggestion.chars().any(|c| !c.is_ascii());
    let meta = lower == "done"
        || lower.starts_with("nothing")
        || lower.starts_with("no suggestion")
        || lower.contains("silent")
        || lower.contains("silence");
    let rejected = suggestion.is_empty()
        || suggestion.contains('\n')
        || suggestion.chars().count() > 120
        || meta
        || (suggestion.starts_with('(') && suggestion.ends_with(')'))
        || (suggestion.starts_with('[') && suggestion.ends_with(']'))
        || suggestion.split_once(": ").is_some_and(|(label, _)| !label.contains(' ') && label.chars().all(char::is_alphanumeric))
        || (words < 2 && !single_word_ok)
        || words > 12
        || ["let me", "i'll", "i will", "here's", "here is"].iter().any(|start| lower.starts_with(start));
    (!rejected).then(|| suggestion.to_owned())
}

/// Generates a suggestion in the background and sends it to the editor.
pub fn spawn(
    provider: claude_core::native_engine::provider_manager::ResolvedProvider,
    input: String,
    sender: std::sync::mpsc::Sender<String>,
) {
    tokio::spawn(async move {
        let generated = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            claude_core::native_engine::generation::generate(provider, SYSTEM, &input),
        )
        .await;
        if let Ok(Ok(text)) = generated {
            if let Some(suggestion) = filter(&text) {
                let _ = sender.send(suggestion);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn suggestions_are_filtered_like_the_reference_cli() {
        assert_eq!(filter("\"run the tests\"").as_deref(), Some("run the tests"));
        assert_eq!(filter("commit this").as_deref(), Some("commit this"));
        assert_eq!(filter("yes").as_deref(), Some("yes"));
        assert_eq!(filter("/compact").as_deref(), Some("/compact"));
        assert_eq!(filter("提交并推送").as_deref(), Some("提交并推送"));
        for rejected in ["", "done", "Nothing to suggest", "(silence)", "Suggestion: run it", "great", "Let me run the tests", "one\ntwo"] {
            assert!(filter(rejected).is_none(), "{rejected:?}");
        }
    }

    #[test]
    fn context_requires_a_finished_answer() {
        let history = vec![json!({"role":"user","content":"fix the bug"}), json!({"role":"assistant","content":"Fixed."})];
        let text = context(&history).unwrap();
        assert!(text.contains("[user]\nfix the bug") && text.contains("SUGGESTION MODE"));
        assert!(context(&history[..1]).is_none());
        assert!(context(&[json!({"role":"assistant","content":""})]).is_none());
    }
}
