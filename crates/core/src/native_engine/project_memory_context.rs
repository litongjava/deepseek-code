//! A compressed window starts with the active project's index. Keep previously
//! attached indexes out of summaries and provider-preserved user items.
use super::*;
use crate::native_engine::openai_client::OpenAIContentPart;

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

/// Responses may preserve visible user items alongside its opaque compaction item.
/// Only omit a host block if the native window already supplies that user text.
pub(super) fn boundary_reminders(
    before: &[String],
    native: &Value,
) -> Option<(usize, OpenAIMessage)> {
    let present = |block: &str| {
        native["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|item| item["role"] == "user")
            .any(|item| {
                let content = &item["content"];
                content.as_str() == Some(block)
                    || content.as_array().into_iter().flatten().any(|part| {
                        part["type"] == "input_text" && part["text"].as_str() == Some(block)
                    })
            })
    };
    let missing: Vec<_> = before
        .iter()
        // The initial Session path is not a compact-time Session index.
        .filter(|block| !block.starts_with("<system-reminder>\n<session_memory_location>"))
        .filter(|block| !present(block))
        .cloned()
        .collect();
    let before_summary = missing
        .iter()
        .any(|block| block.starts_with("<system-reminder>\n<session_memory>"));
    (!missing.is_empty()).then(|| {
        (
            usize::from(!native.is_null() && !before_summary),
            OpenAIMessage {
                role: "user".into(),
                content: OpenAIContent::Multi(
                    missing
                        .into_iter()
                        .map(|text| OpenAIContentPart::Text { text })
                        .collect(),
                ),
                tool_calls: None,
                tool_call_id: None,
                reasoning_content: None,
                provider_content: None,
            },
        )
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
