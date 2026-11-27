use crate::db::session_store::settings::Microcompact;
use serde_json::{json, Value};

pub const MICROCOMPACT_MIN_CHARS: usize = 1000;
pub const CLEARED_TOOL_RESULT: &str = "[Old tool result content cleared to save context]";

fn is_large_tool_content(content: &Value) -> bool {
    match content {
        Value::String(text) => text.len() > MICROCOMPACT_MIN_CHARS,
        Value::Array(blocks) => {
            blocks
                .iter()
                .filter_map(|block| block["text"].as_str())
                .map(str::len)
                .sum::<usize>()
                > MICROCOMPACT_MIN_CHARS
                || blocks.iter().any(|block| block["type"] == "image")
        }
        _ => false,
    }
}

/// Replaces large tool results in `messages` with a short placeholder, in
/// either provider's history format.
pub fn clear_tool_results(messages: &mut [Value]) {
    for message in messages {
        if message["role"] == "tool" && is_large_tool_content(&message["content"]) {
            message["content"] = json!(CLEARED_TOOL_RESULT);
        }
        if let Some(blocks) = message["content"].as_array_mut() {
            for block in blocks
                .iter_mut()
                .filter(|block| block["type"] == "tool_result")
            {
                if is_large_tool_content(&block["content"]) {
                    block["content"] = json!(CLEARED_TOOL_RESULT);
                }
            }
        }
    }
}

fn count_tool_result_slots(messages: &[Value]) -> usize {
    let mut slots = 0usize;
    for message in messages {
        if message["role"] == "tool" {
            slots += 1;
        }
        if let Some(blocks) = message["content"].as_array() {
            slots += blocks
                .iter()
                .filter(|block| block["type"] == "tool_result")
                .count();
        }
    }
    slots
}

/// Clears large tool results in the earliest `limit` tool-result entries across
/// both OpenAI (`role: "tool"`) and Anthropic (`tool_result` content blocks),
/// so a long single-turn Anthropic tool loop can clear older results even when
/// all tool calls share a single replayed `user` message.
fn clear_tool_result_slots(messages: &mut [Value], limit: usize) {
    if limit == 0 {
        return;
    }
    let mut seen = 0usize;
    for message in messages {
        if seen >= limit {
            break;
        }
        if message["role"] == "tool" {
            if is_large_tool_content(&message["content"]) {
                message["content"] = json!(CLEARED_TOOL_RESULT);
            }
            seen += 1;
            if seen >= limit {
                break;
            }
        }
        if let Some(blocks) = message["content"].as_array_mut() {
            for block in blocks
                .iter_mut()
                .filter(|block| block["type"] == "tool_result")
            {
                if seen >= limit {
                    break;
                }
                if is_large_tool_content(&block["content"]) {
                    block["content"] = json!(CLEARED_TOOL_RESULT);
                }
                seen += 1;
            }
        }
    }
}

/// Render the standalone frontend's durable tool-result window. The caller
/// decides whether this is an actual turn that should advance the cursor.
pub fn replay(
    history: &mut [Value],
    state: &mut Microcompact,
    window: Option<u32>,
) -> anyhow::Result<bool> {
    let len = history.len();
    clear_tool_results(&mut history[..state.through.min(len)]);
    clear_tool_result_slots(history, state.cleared_results);
    let estimated = serde_json::to_string(&history)?.len().div_ceil(3);
    let next_through = through(state.through, len, estimated, window);
    let total_slots = count_tool_result_slots(history);
    let next_slots = through(state.cleared_results, total_slots, estimated, window);
    if next_through == state.through && next_slots == state.cleared_results {
        return Ok(false);
    }
    state.through = next_through;
    state.cleared_results = next_slots;
    clear_tool_results(&mut history[..next_through.min(len)]);
    clear_tool_result_slots(history, next_slots);
    Ok(true)
}

pub fn through(current: usize, len: usize, estimated_tokens: usize, window: Option<u32>) -> usize {
    const KEEP_RECENT: usize = 10;
    match window.filter(|window| *window > 0) {
        Some(window) if estimated_tokens > window as usize / 2 => {
            current.max(len.saturating_sub(KEEP_RECENT))
        }
        _ => current,
    }
}
