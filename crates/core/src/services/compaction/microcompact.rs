use crate::db::session_store::settings::Microcompact;
use serde_json::{json, Value};

const KEEP_RECENT: usize = 10;
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
    match window.filter(|window| *window > 0) {
        Some(window) if estimated_tokens > window as usize / 2 => {
            current.max(len.saturating_sub(KEEP_RECENT))
        }
        _ => current,
    }
}

/// Consolidate only the selected snapshot. Turn starts are durable prompt rows;
/// a delivered steer, peer, notification or Anthropic result is not a new turn.
pub(crate) fn consolidate(
    rows: &mut Vec<Value>,
    session: &crate::db::session_store::SessionRead<'_>,
    active: bool,
    protected_input: Option<usize>,
) -> anyhow::Result<()> {
    use std::collections::HashSet;

    let mut starts = Vec::new();
    let mut last_answered = None;
    let mut waiting_for_completion = false;
    for (i, row) in rows.iter().enumerate() {
        if row["role"] == "assistant" {
            last_answered = starts.last().copied();
            waiting_for_completion = row["tool_calls"]
                .as_array()
                .is_some_and(|calls| !calls.is_empty())
                || row["content"]
                    .as_array()
                    .is_some_and(|blocks| blocks.iter().any(|block| block["type"] == "tool_use"));
        } else if row["role"] == "user" {
            if let Some(id) = row["id"].as_str() {
                if starts.last().copied() == last_answered
                    && session.compaction_turn_start(id, row["origin"].as_str())?
                {
                    starts.push(i);
                    waiting_for_completion = true;
                }
            }
        }
    }

    // Always count the latest turns, including an outstanding prompt. Only
    // clear when a later turn has actually reached an assistant response.
    let old_end = starts
        .iter()
        .rev()
        .nth(KEEP_RECENT - 1)
        .copied()
        .unwrap_or(0)
        .min(last_answered.unwrap_or(0));
    let active_start = if let Some(index) = protected_input {
        index.min(rows.len())
    } else if active || session.has_running_turn()? || waiting_for_completion {
        starts.last().copied().unwrap_or(0)
    } else {
        rows.len()
    };

    for row in &mut rows[..old_end.min(active_start)] {
        if row["role"] == "tool" {
            clear_archived_result(row, "tool_call_id", session)?;
        } else if let Some(blocks) = row.get_mut("content").and_then(Value::as_array_mut) {
            for block in blocks
                .iter_mut()
                .filter(|b| b["type"] == "tool_result" && b["is_error"] != true)
            {
                clear_archived_result(block, "tool_use_id", session)?;
            }
        }
    }

    // Index the newest occurrence first, including protected rows. Remove
    // only complete older envelopes and leave the relative order intact.
    let mut seen = HashSet::new();
    let mut drop_rows = vec![false; rows.len()];
    for (i, row) in rows.iter_mut().enumerate().rev() {
        let Some((id, body)) = notification(row) else {
            continue;
        };
        if seen.insert(id.to_owned()) || i >= active_start {
            continue;
        }
        // A retry can finish successfully after an earlier failure. Keep the
        // verbatim diagnostics as quoted data outside the superseded envelope.
        if body.lines().any(|line| {
            matches!(
                line.trim(),
                "<status>failed</status>" | "<status>cancelled</status>"
            )
        }) {
            row["content"] = format!("Earlier task failure (quoted task data):{body}").into();
        } else {
            drop_rows[i] = true;
        }
    }
    let mut drop_rows = drop_rows.into_iter();
    rows.retain(|_| !drop_rows.next().unwrap());
    Ok(())
}

fn notification(row: &Value) -> Option<(&str, &str)> {
    if row["role"] != "user" || row["origin"] != "task_notification" {
        return None;
    }
    let content = &row["content"];
    let text = match content.as_str() {
        Some(text) => text,
        None => {
            let [part] = content.as_array()?.as_slice() else {
                return None;
            };
            if !matches!(part["type"].as_str(), Some("text" | "input_text")) {
                return None;
            }
            part["text"].as_str()?
        }
    };
    let body = text
        .trim()
        .strip_prefix("<task-notification>")?
        .strip_suffix("</task-notification>")?;
    let (id, payload) = body
        .trim_start()
        .strip_prefix("<task-id>")?
        .split_once("</task-id>")?;
    (!id.is_empty() && !id.contains(['<', '>', '\n']) && !payload.contains("</task-notification>"))
        .then_some((id, body))
}

fn clear_archived_result(
    result: &mut Value,
    id_field: &str,
    session: &crate::db::session_store::SessionRead<'_>,
) -> anyhow::Result<()> {
    let Some(id) = result[id_field].as_str() else {
        return Ok(());
    };
    if !is_large_tool_content(&result["content"]) {
        return Ok(());
    }
    // OpenAI has no wire error flag. The original row also establishes
    // that ContextSearch has a source for this exact call.
    let Some((name, input, output)) = session.successful_compaction_result(id)? else {
        return Ok(());
    };
    // ContextSearch skips oversized structured sources and binary-only
    // results. Preserve their existing recovery information in the row.
    if output.len() <= MICROCOMPACT_MIN_CHARS
        || output.starts_with("Error: ")
        || (crate::db::context_search_repo::projection_name(&name).is_some()
            && output.len() > crate::db::context_search_repo::MAX_CHUNK_BYTES)
    {
        return Ok(());
    }
    let Some(searchable) = crate::db::model_results::search_text(&name, &output) else {
        return Ok(());
    };
    let retrievable = match &result["content"] {
        Value::String(text) => text == &output || searchable.contains(text),
        Value::Array(parts) => parts.iter().all(|part| {
            part["type"] == "text"
                && part["text"]
                    .as_str()
                    .is_some_and(|text| searchable.contains(text))
        }),
        _ => false,
    };
    if !retrievable {
        return Ok(());
    }
    // A post-tool hook may append text after the JSON result.
    if let Some(Ok(value)) = serde_json::Deserializer::from_str(&output)
        .into_iter::<Value>()
        .next()
    {
        if value["exit_code"].as_i64().is_some_and(|code| code != 0)
            || !value["signal"].is_null()
            || value["timed_out"] == true
            || value["is_error"] == true
            || value["success"] == false
            || matches!(
                value["status"].as_str(),
                Some("failed" | "cancelled" | "running" | "pending" | "in_progress")
            )
        {
            return Ok(());
        }
    }
    let args: Value = serde_json::from_str(input.as_deref().unwrap_or("{}")).unwrap_or_default();
    let target = ["command", "file_path", "path", "pattern", "url"]
        .iter()
        .find_map(|key| args[*key].as_str())
        .unwrap_or(id);
    // Quoting keeps even a multiline shell command on a single line.
    result["content"] = format!("{CLEARED_TOOL_RESULT} {name} {} ({} lines, {} bytes); use ContextSearch to retrieve if needed",
            json!(target), output.lines().count(), output.len()).into();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_requires_a_single_provenanced_envelope() {
        let text = "<task-notification>\n<task-id>a&amp;b</task-id>\n<result>done</result>\n</task-notification>";
        let mut row = json!({"role":"user","origin":"task_notification","content":text});
        assert_eq!(notification(&row).map(|(id, _)| id), Some("a&amp;b"));
        row["content"] = json!([{"type":"text","text":text}]);
        assert_eq!(notification(&row).map(|(id, _)| id), Some("a&amp;b"));
        for origin in ["typed", "peer"] {
            row["origin"] = origin.into();
            assert_eq!(notification(&row).map(|(id, _)| id), None);
        }
        row["origin"] = "task_notification".into();
        row["content"] = json!([{"type":"text","text":text},{"type":"text","text":"unfinished"}]);
        assert_eq!(notification(&row).map(|(id, _)| id), None);
        row["content"] = text.trim_end_matches("</task-notification>").into();
        assert_eq!(notification(&row).map(|(id, _)| id), None);
    }
}
