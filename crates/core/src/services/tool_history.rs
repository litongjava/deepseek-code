//! Projection from engine tool events to the durable `tool_calls` rows.
//!
//! This is the one product path hosts use to turn `ToolUseStart` / `ToolUseDone`
//! (and the ToolScript origin events) into rows. The CLI's live transcript and
//! the engine integration tests both call it, so a test does not hand-write SQL
//! that the product never executes.

use crate::db::message_repo::{self, ToolCallRow};
use crate::native_engine::tool_loop::EngineEvent;
use anyhow::Result;
use rusqlite::Connection;
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

/// `tool_use_id -> (parent_tool_use_id, sequence)` for nested ToolScript calls.
pub type ToolOrigins = BTreeMap<String, (String, u64)>;

/// Applies one event to the in-memory turn state. Returns whether anything
/// changed and therefore needs a flush.
///
/// Argument deltas accumulate independently from the row, exactly as the CLI's
/// streaming transcript did: a delta or start arriving after the completion
/// never overwrites the completed call's effective input, and a legacy
/// completion without input never erases a richer input already seen.
pub fn apply_event(
    tools: &mut Vec<ToolCallRow>,
    origins: &mut ToolOrigins,
    arguments: &mut std::collections::HashMap<String, String>,
    event: &EngineEvent,
) -> bool {
    match event {
        EngineEvent::ToolArgDelta { tool_use_id, delta } => {
            let accumulated = arguments.entry(tool_use_id.clone()).or_default();
            accumulated.push_str(delta);
            let incoming = accumulated.clone();
            let index = row_index(tools, tool_use_id);
            set_input(&mut tools[index], incoming, false);
            true
        }
        EngineEvent::ToolUseStart {
            tool_use_id,
            tool_name,
            tool_input,
            ..
        } => {
            let index = row_index(tools, tool_use_id);
            let row = &mut tools[index];
            row.name = tool_name.clone();
            set_input(row, tool_input.to_string(), false);
            true
        }
        EngineEvent::ToolUseDone {
            tool_use_id,
            tool_name,
            tool_input,
            output,
            is_error,
        }
        | EngineEvent::PipelineToolResult {
            tool_use_id,
            tool_name,
            tool_input,
            output,
            is_error,
            ..
        } => {
            let index = row_index(tools, tool_use_id);
            let row = &mut tools[index];
            row.name = tool_name.clone();
            set_input(row, tool_input.to_string(), true);
            row.output = Some(output.clone());
            row.is_error = *is_error;
            true
        }
        EngineEvent::NestedToolCall {
            tool_use_id,
            parent_tool_use_id,
            sequence,
        } => {
            // First observation wins: a conflicting replay must not reparent an
            // existing occurrence.
            origins
                .entry(tool_use_id.clone())
                .or_insert_with(|| (parent_tool_use_id.clone(), *sequence));
            true
        }
        _ => false,
    }
}

/// The shared late-event rule. `completed` means the event carries a result, so
/// it may replace a previous input; a delta or a late start may not.
fn set_input(row: &mut ToolCallRow, incoming: String, completed: bool) {
    let existing = row.input.clone().unwrap_or_default();
    if (row.output.is_none() || completed) && (rich(&incoming) || !rich(&existing)) {
        row.input = Some(incoming);
    }
}

fn row_index(tools: &mut Vec<ToolCallRow>, id: &str) -> usize {
    tools
        .iter()
        .position(|tool| tool.id == id)
        .unwrap_or_else(|| {
            tools.push(ToolCallRow {
                id: id.to_owned(),
                message_id: String::new(),
                name: String::new(),
                input: None,
                output: None,
                is_error: false,
                sort_order: tools.len() as i64,
            });
            tools.len() - 1
        })
}

fn rich(text: &str) -> bool {
    serde_json::from_str::<Value>(text)
        .ok()
        .is_some_and(|value| value.as_object().is_some_and(|fields| !fields.is_empty()))
}

/// Writes the assistant message and every observed call, keyed by the durable
/// `{assistant}:{occurrence}` id.
///
/// A nested call's row and origin are written in one transaction, so the two
/// are never separated by a crash. An origin whose row has not been observed
/// yet is not persisted: the next flush writes both together.
pub fn save(
    conn: &Connection,
    conversation_id: &str,
    assistant_id: &str,
    text: &str,
    tools: &[ToolCallRow],
    origins: &ToolOrigins,
) -> Result<()> {
    for tool in tools {
        let qualified = format!("{assistant_id}:{}", tool.id);
        let row = ToolCallRow {
            id: qualified,
            message_id: assistant_id.to_owned(),
            ..tool.clone()
        };
        match origins.get(&tool.id) {
            Some((parent, sequence)) => {
                let parent = format!("{assistant_id}:{parent}");
                message_repo::save_stream_nested_tool_result(
                    conn,
                    conversation_id,
                    assistant_id,
                    text,
                    &row,
                    &parent,
                    *sequence as i64,
                )?;
            }
            None => {
                message_repo::save_stream_tool_result(
                    conn,
                    conversation_id,
                    assistant_id,
                    text,
                    &row,
                )?;
            }
        }
    }
    Ok(())
}

/// Occurrence ids that belong to an embedded script and must not become
/// provider protocol items on replay.
pub fn nested_ids_for_message(conn: &Connection, message_id: &str) -> Result<HashSet<String>> {
    let mut stmt = conn.prepare_cached(
        "SELECT o.tool_call_id FROM tool_call_origins o \
         JOIN tool_calls t ON t.id = o.tool_call_id WHERE t.message_id = ?1",
    )?;
    let rows = stmt.query_map([message_id], |row| row.get::<_, String>(0))?;
    let mut ids = HashSet::new();
    for row in rows {
        ids.insert(row?);
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema::SCHEMA_SQL;
    use serde_json::json;

    fn conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        conn.execute_batch(SCHEMA_SQL).unwrap();
        crate::db::conversation_repo::insert_conversation(
            &conn, "s", None, None, None, None, None, false, false, false, "now", "now", 1,
        )
        .unwrap();
        message_repo::insert_message(&conn, "a", "s", "assistant", "", None, "now", false, 0)
            .unwrap();
        conn
    }

    #[test]
    fn nested_origins_save_once_and_cascade_with_the_message() {
        let conn = conn();
        message_repo::insert_tool_call(
            &conn,
            "a:script",
            "a",
            "ToolScript",
            Some("{}"),
            Some("done"),
            false,
            0,
        )
        .unwrap();
        message_repo::insert_tool_call(
            &conn,
            "a:child",
            "a",
            "Read",
            Some("{}"),
            Some("text"),
            false,
            1,
        )
        .unwrap();
        for _ in 0..2 {
            message_repo::save_tool_origin(&conn, "a:child", "a:script", 1).unwrap();
        }
        let origins = message_repo::get_tool_origins_for_message(&conn, "a").unwrap();
        assert_eq!(origins.len(), 1);
        assert_eq!(origins[0], ("a:child".to_owned(), "a:script".to_owned(), 1));
        assert_eq!(
            nested_ids_for_message(&conn, "a").unwrap(),
            ["a:child".to_owned()].into_iter().collect()
        );
        message_repo::delete_message(&conn, "a").unwrap();
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM tool_call_origins", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(remaining, 0, "message delete must cascade to origins");
    }

    #[test]
    fn live_events_keep_original_then_effective_input_and_drain_once() {
        let mut tools = Vec::new();
        let mut origins = ToolOrigins::new();
        let mut arguments = std::collections::HashMap::new();
        let start = EngineEvent::ToolUseStart {
            tool_use_id: "inv-1".into(),
            tool_name: "Write".into(),
            tool_input: json!({"file_path":"original.txt"}),
            text_before: String::new(),
            display_title: None,
        };
        assert!(apply_event(
            &mut tools,
            &mut origins,
            &mut arguments,
            &start
        ));
        let done = EngineEvent::ToolUseDone {
            tool_use_id: "inv-1".into(),
            tool_name: "Write".into(),
            tool_input: json!({"file_path":"hooked.txt"}),
            output: "ok".into(),
            is_error: false,
        };
        assert!(apply_event(&mut tools, &mut origins, &mut arguments, &done));
        let origin = EngineEvent::NestedToolCall {
            tool_use_id: "inv-1".into(),
            parent_tool_use_id: "inv-script".into(),
            sequence: 1,
        };
        assert!(apply_event(
            &mut tools,
            &mut origins,
            &mut arguments,
            &origin
        ));
        assert_eq!(tools.len(), 1);
        assert_eq!(
            serde_json::from_str::<Value>(tools[0].input.as_deref().unwrap()).unwrap(),
            json!({"file_path":"hooked.txt"})
        );
        assert_eq!(origins["inv-1"], ("inv-script".to_owned(), 1));
        assert!(!apply_event(
            &mut tools,
            &mut origins,
            &mut arguments,
            &EngineEvent::Text("ignored".into())
        ));
    }

    #[test]
    fn late_deltas_and_starts_do_not_rewrite_a_completed_call() {
        let mut tools = Vec::new();
        let mut origins = ToolOrigins::new();
        let mut arguments = std::collections::HashMap::new();
        let delta = EngineEvent::ToolArgDelta {
            tool_use_id: "late".into(),
            delta: r#"{"file_path":"partial"#.into(),
        };
        assert!(apply_event(
            &mut tools,
            &mut origins,
            &mut arguments,
            &delta
        ));
        let done = EngineEvent::ToolUseDone {
            tool_use_id: "late".into(),
            tool_name: "Write".into(),
            tool_input: json!({"file_path":"complete.txt"}),
            output: "ok".into(),
            is_error: false,
        };
        assert!(apply_event(&mut tools, &mut origins, &mut arguments, &done));
        // A late delta and a late start after completion must not change input.
        apply_event(
            &mut tools,
            &mut origins,
            &mut arguments,
            &EngineEvent::ToolArgDelta {
                tool_use_id: "late".into(),
                delta: r#"{"file_path":"evil.txt"}"#.into(),
            },
        );
        apply_event(
            &mut tools,
            &mut origins,
            &mut arguments,
            &EngineEvent::ToolUseStart {
                tool_use_id: "late".into(),
                tool_name: "Write".into(),
                tool_input: json!({"file_path":"late-start.txt"}),
                text_before: String::new(),
                display_title: None,
            },
        );
        assert_eq!(
            serde_json::from_str::<Value>(tools[0].input.as_deref().unwrap()).unwrap(),
            json!({"file_path":"complete.txt"})
        );
        // A later completion may still replace it, as before.
        apply_event(
            &mut tools,
            &mut origins,
            &mut arguments,
            &EngineEvent::ToolUseDone {
                tool_use_id: "late".into(),
                tool_name: "Write".into(),
                tool_input: json!({"file_path":"final.txt"}),
                output: "ok".into(),
                is_error: false,
            },
        );
        assert_eq!(
            serde_json::from_str::<Value>(tools[0].input.as_deref().unwrap()).unwrap(),
            json!({"file_path":"final.txt"})
        );
    }

    #[test]
    fn a_conflicting_origin_replay_does_not_reparent() {
        let conn = conn();
        message_repo::insert_tool_call(
            &conn,
            "a:child",
            "a",
            "Read",
            Some("{}"),
            Some("text"),
            false,
            0,
        )
        .unwrap();
        message_repo::save_tool_origin(&conn, "a:child", "a:first", 1).unwrap();
        message_repo::save_tool_origin(&conn, "a:child", "a:second", 9).unwrap();
        let origins = message_repo::get_tool_origins_for_message(&conn, "a").unwrap();
        assert_eq!(
            origins,
            vec![("a:child".to_owned(), "a:first".to_owned(), 1)]
        );
    }

    #[test]
    fn nested_row_and_origin_are_written_atomically_or_not_at_all() {
        let conn = conn();
        // Force the origin insert to abort after the row insert inside the same
        // transaction, simulating the crash boundary.
        conn.execute_batch(
            "CREATE TRIGGER fail_origin BEFORE INSERT ON tool_call_origins \
             WHEN NEW.tool_call_id = 'a:child' BEGIN SELECT RAISE(ABORT, 'fixture'); END;",
        )
        .unwrap();
        let tool = ToolCallRow {
            id: "a:child".into(),
            message_id: "a".into(),
            name: "Read".into(),
            input: Some("{}".into()),
            output: Some("text".into()),
            is_error: false,
            sort_order: 0,
        };
        let failed =
            message_repo::save_stream_nested_tool_result(&conn, "s", "a", "", &tool, "a:script", 1);
        assert!(failed.is_err(), "the origin failure must abort the write");
        let rows = message_repo::get_tool_calls_for_message(&conn, "a").unwrap();
        assert!(rows.is_empty(), "the row must roll back with the origin");
        conn.execute_batch("DROP TRIGGER fail_origin;").unwrap();
        message_repo::save_stream_nested_tool_result(&conn, "s", "a", "", &tool, "a:script", 1)
            .unwrap();
        assert_eq!(
            message_repo::get_tool_calls_for_message(&conn, "a")
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            message_repo::get_tool_origins_for_message(&conn, "a").unwrap(),
            vec![("a:child".to_owned(), "a:script".to_owned(), 1)]
        );
    }

    #[test]
    fn an_origin_without_its_row_is_deferred_not_lost() {
        let conn = conn();
        message_repo::insert_tool_call(
            &conn,
            "a:script",
            "a",
            "ToolScript",
            Some("{}"),
            Some("done"),
            false,
            0,
        )
        .unwrap();
        let mut origins = ToolOrigins::new();
        origins.insert("child".into(), ("script".into(), 1));
        // Start alone is not durable yet: one flush with the origin but no row.
        save(&conn, "s", "a", "", &[], &origins).unwrap();
        assert!(message_repo::get_tool_origins_for_message(&conn, "a")
            .unwrap()
            .is_empty());
        let rows = vec![ToolCallRow {
            id: "child".into(),
            message_id: "a".into(),
            name: "Read".into(),
            input: Some("{}".into()),
            output: Some("text".into()),
            is_error: false,
            sort_order: 1,
        }];
        save(&conn, "s", "a", "", &rows, &origins).unwrap();
        assert_eq!(
            message_repo::get_tool_origins_for_message(&conn, "a").unwrap(),
            vec![("a:child".to_owned(), "a:script".to_owned(), 1)]
        );
    }
}
