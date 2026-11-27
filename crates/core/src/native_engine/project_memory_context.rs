//! A compressed window starts with the active project's index. Keep previously
//! attached indexes out of summaries and provider-preserved user items.
use super::*;

pub(super) fn is_index(block: &str) -> bool {
    block
        .strip_prefix("<system-reminder>\n")
        .is_some_and(|body| {
            body.starts_with("MEMORY.md changed.")
                || ["global", "team", "project", "subagent", "session"]
                    .iter()
                    .any(|scope| body.starts_with(&format!("<{scope}_memory>\n")))
        })
}

impl ToolLoopExecutor {
    pub(super) fn remove_compaction_indexes(&self, messages: &mut Vec<Value>) {
        let indexes: Vec<_> = self
            .context_reminders
            .iter()
            .chain(self.turn_context.iter().flat_map(|entry| &entry.blocks))
            .filter(|s| is_index(s))
            .collect();
        if indexes.is_empty() {
            return;
        }
        messages.retain_mut(|m| {
            if m["role"] != "user" || !m["provider_content"].is_null() {
                return true;
            }
            match &mut m["content"] {
                Value::Array(parts) => {
                    parts.retain(|p| {
                        !indexes
                            .iter()
                            .any(|s| p["text"].as_str() == Some(s.as_str()))
                    });
                    !parts.is_empty()
                }
                Value::String(text) => {
                    for index in &indexes {
                        if text == *index {
                            return false;
                        }
                        // Chat Completions joins the prefix, user text and turn
                        // attachments with blank lines.
                        if let Some(at) = text.find(index.as_str()) {
                            let end = at + index.len();
                            if (at == 0 || text[..at].ends_with("\n\n"))
                                && (end == text.len() || text[end..].starts_with("\n\n"))
                            {
                                if end < text.len() {
                                    text.replace_range(at..end + 2, "");
                                } else {
                                    text.truncate(at.saturating_sub(2));
                                }
                            }
                        }
                    }
                    true
                }
                _ => true,
            }
        });
    }
}
