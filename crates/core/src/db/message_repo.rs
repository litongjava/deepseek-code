use anyhow::Result;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

/// Request snapshots belong to the assistant message and follow its deletion lifecycle.
///
/// Repeated large headers (tools and system prompts are identical across most
/// requests of a session) are stored once in `request_blobs` and referenced by
/// hash; the caller's value is left untouched for live display.
pub fn save_request_snapshots(
    conn: &Connection,
    message_id: &str,
    snapshots: &serde_json::Value,
) -> Result<()> {
    let stored = store_trajectory_blobs(conn, snapshots)?;
    conn.execute("INSERT INTO message_trajectory (message_id, requests_json) VALUES (?1, ?2) ON CONFLICT(message_id) DO UPDATE SET requests_json = excluded.requests_json",
        params![message_id, serde_json::to_string(&stored)?])?;
    if let Some(requests) = stored.as_array() {
        for request in requests {
            super::request_repo::save_request(conn, message_id, request)?;
        }
    }
    Ok(())
}

pub fn get_request_snapshots(conn: &Connection, message_id: &str) -> Result<serde_json::Value> {
    use rusqlite::OptionalExtension;
    let raw: Option<String> = conn
        .query_row(
            "SELECT requests_json FROM message_trajectory WHERE message_id = ?1",
            params![message_id],
            |row| row.get(0),
        )
        .optional()?;
    let mut snapshots: serde_json::Value = match raw {
        Some(raw) => serde_json::from_str(&raw)?,
        None => serde_json::json!([]),
    };
    if let Some(items) = snapshots.as_array_mut() {
        for item in items {
            load_trajectory_blobs(conn, item)?;
        }
    }
    Ok(snapshots)
}

/// Header fields shared by most requests of a session, chosen by measured size
/// (`tools` 79.6%, `systemPrompt` 16.3%, `systemMessages` 1.0% of requests_json).
const TRAJECTORY_BLOB_FIELDS: [&str; 3] = ["systemPrompt", "tools", "systemMessages"];

fn trajectory_blob_ref(field: &str) -> String {
    format!("{field}Ref")
}

fn content_hash(body: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(body.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Replaces repeated headers with `*Ref` keys and stores each distinct body
/// once. Returns a copy; the caller keeps the full payload for display.
fn store_trajectory_blobs(
    conn: &Connection,
    snapshots: &serde_json::Value,
) -> Result<serde_json::Value> {
    let Some(items) = snapshots.as_array() else {
        return Ok(snapshots.clone());
    };
    let mut stored = Vec::with_capacity(items.len());
    for item in items {
        let mut item = item.clone();
        if let Some(object) = item.as_object_mut() {
            for field in TRAJECTORY_BLOB_FIELDS {
                let ref_key = trajectory_blob_ref(field);
                let Some(value) = object.remove(field) else {
                    continue;
                };
                object.remove(&ref_key);
                if value.is_null() {
                    // Keep an explicit null inline so the round trip is exact.
                    object.insert(field.to_string(), value);
                    continue;
                }
                let body = serde_json::to_string(&value)?;
                let hash = content_hash(&body);
                conn.execute(
                    "INSERT INTO request_blobs(hash, body) VALUES (?1, ?2) ON CONFLICT(hash) DO NOTHING",
                    params![hash, body],
                )?;
                object.insert(ref_key, serde_json::Value::String(hash));
            }
        }
        stored.push(item);
    }
    Ok(serde_json::Value::Array(stored))
}

/// Restores inline fields from their references. A reference whose body is gone
/// is kept as is, so callers can tell the content is missing instead of seeing
/// invented data.
pub fn load_trajectory_blobs(conn: &Connection, snapshot: &mut serde_json::Value) -> Result<()> {
    use rusqlite::OptionalExtension;
    let Some(object) = snapshot.as_object_mut() else {
        return Ok(());
    };
    for field in TRAJECTORY_BLOB_FIELDS {
        let ref_key = trajectory_blob_ref(field);
        let Some(hash) = object
            .get(&ref_key)
            .and_then(|value| value.as_str())
            .map(str::to_owned)
        else {
            continue;
        };
        let body: Option<String> = conn
            .query_row(
                "SELECT body FROM request_blobs WHERE hash = ?1",
                params![hash],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(body) = body {
            object.insert(field.to_string(), serde_json::from_str(&body)?);
            object.remove(&ref_key);
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageRow {
    pub id: String,
    pub conversation_id: String,
    pub role: String,
    pub content: String,
    pub thinking: Option<String>,
    pub created_at: String,
    pub is_compact_boundary: bool,
    pub sort_order: i64,
}

fn row_to_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<MessageRow> {
    Ok(MessageRow {
        id: row.get(0)?,
        conversation_id: row.get(1)?,
        role: row.get(2)?,
        content: row.get(3)?,
        thinking: row.get(4)?,
        created_at: row.get(5)?,
        is_compact_boundary: row.get::<_, i64>(6)? != 0,
        sort_order: row.get(7)?,
    })
}

pub fn insert_message(
    conn: &Connection,
    id: &str,
    conversation_id: &str,
    role: &str,
    content: &str,
    thinking: Option<&str>,
    created_at: &str,
    is_compact_boundary: bool,
    sort_order: i64,
) -> Result<()> {
    let mut stmt = conn.prepare_cached(
        "INSERT INTO messages (id, conversation_id, role, content, thinking, created_at, is_compact_boundary, sort_order) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"
    )?;
    stmt.execute(params![
        id,
        conversation_id,
        role,
        content,
        thinking,
        created_at,
        is_compact_boundary as i64,
        sort_order,
    ])?;
    Ok(())
}

/// Save display text and durable multimodal content inside the caller's transaction.
pub fn save_user_message(
    conn: &Connection,
    id: &str,
    conversation_id: &str,
    content: &serde_json::Value,
    created_at: &str,
    sort_order: i64,
) -> Result<()> {
    // Keep ordinary text (including historical empty messages) compatible.
    let normalized = if content.is_string() {
        content.clone()
    } else {
        crate::runtime::normalize_user_content(content)?
    };
    let display = match normalized.as_str() {
        Some(text) => text.to_owned(),
        None => normalized
            .as_array()
            .unwrap()
            .iter()
            .map(|block| {
                if block["type"] == "text" {
                    block["text"].as_str().unwrap_or("")
                } else if block["type"] == "attachment" {
                    block["fileName"].as_str().unwrap_or("[File]")
                } else {
                    "[Image]"
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
    };
    let changed = conn.execute(
        "INSERT INTO messages (id, conversation_id, role, content, created_at, sort_order) VALUES (?1, ?2, 'user', ?3, ?4, ?5) ON CONFLICT(id) DO UPDATE SET content = excluded.content, sort_order = excluded.sort_order WHERE messages.conversation_id = excluded.conversation_id AND messages.role = 'user'",
        params![id, conversation_id, display, created_at, sort_order],
    )?;
    anyhow::ensure!(
        changed == 1,
        "Message ID belongs to a different conversation or role"
    );
    if normalized.is_array() {
        conn.execute(
            "INSERT INTO message_user_content (message_id, content_json) VALUES (?1, ?2) ON CONFLICT(message_id) DO UPDATE SET content_json = excluded.content_json",
            params![id, serde_json::to_string(&normalized)?],
        )?;
    } else {
        conn.execute(
            "DELETE FROM message_user_content WHERE message_id = ?1",
            params![id],
        )?;
    }
    Ok(())
}

/// UI history must remain readable even if an old sidecar is corrupted.
pub fn user_content_for_display(
    conn: &Connection,
    message: &MessageRow,
) -> Result<serde_json::Value> {
    use rusqlite::OptionalExtension;
    if message.role != "user" {
        return Ok(serde_json::json!(message.content));
    }
    let raw: Option<String> = conn
        .query_row(
            "SELECT content_json FROM message_user_content WHERE message_id = ?1",
            params![message.id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(raw) = raw else {
        return Ok(serde_json::json!(message.content));
    };
    let normalized = serde_json::from_str::<serde_json::Value>(&raw)
        .ok()
        .and_then(|content| crate::runtime::normalize_user_content(&content).ok());
    Ok(normalized.unwrap_or_else(|| serde_json::json!([
        {"type":"text", "text":message.content},
        {"type":"image_unavailable", "reason":"Saved image content is unavailable or corrupted."}
    ])))
}

pub fn get_messages_by_conversation(
    conn: &Connection,
    conversation_id: &str,
) -> Result<Vec<MessageRow>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, conversation_id, role, content, thinking, created_at, is_compact_boundary, sort_order FROM messages WHERE conversation_id = ?1 ORDER BY sort_order ASC"
    )?;
    let rows = stmt.query_map(params![conversation_id], |row| row_to_message(row))?;
    let mut result = Vec::new();
    for row in rows {
        result.push(row?);
    }
    Ok(result)
}

pub fn get_message(conn: &Connection, id: &str) -> Result<Option<MessageRow>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, conversation_id, role, content, thinking, created_at, is_compact_boundary, sort_order FROM messages WHERE id = ?1"
    )?;
    let mut rows = stmt.query(params![id])?;
    match rows.next()? {
        Some(row) => Ok(Some(row_to_message(row)?)),
        None => Ok(None),
    }
}

pub fn delete_message(conn: &Connection, id: &str) -> Result<()> {
    let mut stmt = conn.prepare_cached("DELETE FROM messages WHERE id = ?1")?;
    stmt.execute(params![id])?;
    Ok(())
}

pub fn delete_messages_from(
    conn: &Connection,
    conversation_id: &str,
    sort_order: i64,
) -> Result<()> {
    let mut stmt = conn
        .prepare_cached("DELETE FROM messages WHERE conversation_id = ?1 AND sort_order >= ?2")?;
    stmt.execute(params![conversation_id, sort_order])?;
    Ok(())
}

pub fn delete_messages_tail(conn: &Connection, conversation_id: &str, count: i64) -> Result<()> {
    let mut stmt = conn.prepare_cached(
        "DELETE FROM messages WHERE conversation_id = ?1 AND sort_order >= (SELECT MIN(sort_order) FROM (SELECT sort_order FROM messages WHERE conversation_id = ?1 ORDER BY sort_order DESC LIMIT ?2))"
    )?;
    stmt.execute(params![conversation_id, count])?;
    Ok(())
}

pub fn update_message_content(conn: &Connection, id: &str, content: &str) -> Result<()> {
    let mut stmt = conn.prepare_cached("UPDATE messages SET content = ?1 WHERE id = ?2")?;
    stmt.execute(params![content, id])?;
    Ok(())
}

/// What the model reads for a call whose process ended before it reported.
pub const UNFINISHED_TOOL_RESULT: &str = "The tool did not finish: the session ended before a result was recorded, so its outcome is unknown. Check the current state before repeating it.";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallRow {
    pub id: String,
    pub message_id: String,
    pub name: String,
    pub input: Option<String>,
    pub output: Option<String>,
    pub is_error: bool,
    pub sort_order: i64,
}

/// Records the note the user added when approving a stored call.
pub fn set_tool_call_note(conn: &Connection, tool_call_id: &str, note: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO tool_call_notes (tool_call_id, note) VALUES (?1, ?2) ON CONFLICT(tool_call_id) DO UPDATE SET note = excluded.note",
        rusqlite::params![tool_call_id, note],
    )?;
    Ok(())
}

pub fn tool_call_note(conn: &Connection, tool_call_id: &str) -> Result<Option<String>> {
    use rusqlite::OptionalExtension;
    Ok(conn
        .query_row(
            "SELECT note FROM tool_call_notes WHERE tool_call_id = ?1",
            [tool_call_id],
            |row| row.get(0),
        )
        .optional()?)
}

pub fn get_tool_calls_for_message(conn: &Connection, message_id: &str) -> Result<Vec<ToolCallRow>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, message_id, name, input, output, is_error, sort_order FROM tool_calls WHERE message_id = ?1 ORDER BY sort_order ASC"
    )?;
    let rows = stmt.query_map(params![message_id], |row| {
        Ok(ToolCallRow {
            id: row.get(0)?,
            message_id: row.get(1)?,
            name: row.get(2)?,
            input: row.get(3)?,
            output: row.get(4)?,
            is_error: row.get::<_, i64>(5)? != 0,
            sort_order: row.get(6)?,
        })
    })?;
    let mut result = Vec::new();
    for row in rows {
        result.push(row?);
    }
    Ok(result)
}

pub fn insert_tool_call(
    conn: &Connection,
    id: &str,
    message_id: &str,
    name: &str,
    input: Option<&str>,
    output: Option<&str>,
    is_error: bool,
    sort_order: i64,
) -> Result<()> {
    let mut stmt = conn.prepare_cached(
        "INSERT INTO tool_calls (id, message_id, name, input, output, is_error, sort_order) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) ON CONFLICT(id) DO UPDATE SET message_id=excluded.message_id, name=excluded.name, input=excluded.input, output=excluded.output, is_error=excluded.is_error, sort_order=excluded.sort_order"
    )?;
    stmt.execute(params![
        id,
        message_id,
        name,
        input,
        output,
        is_error as i64,
        sort_order,
    ])?;
    // CLI/archive rows qualify provider IDs with their assistant-message ID.
    // Bind the typed MCP sidecar to that durable ID in the same save operation.
    if let Some(original) = id.strip_prefix(&format!("{message_id}:")) {
        conn.execute(
            "INSERT OR REPLACE INTO mcp_tool_outputs (conversation_id,tool_use_id,content_json,metadata_json,display_text,is_error) \
             SELECT conversation_id,?1,content_json,metadata_json,display_text,is_error FROM mcp_tool_outputs \
             WHERE conversation_id=(SELECT conversation_id FROM messages WHERE id=?2) AND tool_use_id=?3 AND display_text=?4 AND is_error=?5",
            params![id,message_id,original,output,is_error],
        )?;
    }
    Ok(())
}

/// Save each result as it arrives, rather than waiting for the whole assistant turn.
pub fn save_stream_tool_result(
    conn: &Connection,
    conversation_id: &str,
    message_id: &str,
    content: &str,
    tool: &ToolCallRow,
) -> Result<()> {
    let tx = crate::db::write_transaction(&conn)?;
    tx.execute("INSERT INTO messages (id, conversation_id, role, content, created_at, sort_order)
        VALUES (?1, ?2, 'assistant', ?3, ?4, (SELECT COALESCE(MAX(sort_order), -1) + 1 FROM messages WHERE conversation_id = ?2))
        ON CONFLICT(id) DO UPDATE SET content = excluded.content",
        params![message_id, conversation_id, content, chrono::Utc::now().to_rfc3339()])?;
    insert_tool_call(
        &tx,
        &tool.id,
        message_id,
        &tool.name,
        tool.input.as_deref(),
        tool.output.as_deref(),
        tool.is_error,
        tool.sort_order,
    )?;
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod stream_tool_tests {
    use super::*;

    #[test]
    fn qualified_tool_ids_keep_mcp_content_without_crossing_session_or_output_identity() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::db::schema::SCHEMA_SQL).unwrap();
        for session in ["one", "two"] {
            crate::db::conversation_repo::insert_conversation(
                &conn, session, None, None, None, None, None, false, false, false, "now", "now", 0,
            )
            .unwrap();
            insert_message(
                &conn,
                session,
                session,
                "assistant",
                "",
                None,
                "now",
                false,
                0,
            )
            .unwrap();
        }
        let content = crate::native_engine::mcp_output::McpOutput::from_transport(&serde_json::json!({"content":[{"type":"text","text":"final content"}],"_meta":{"trace":"preserved"}})).unwrap();
        let display = content.display();
        content.save(&conn, "one", "call", &display, false).unwrap();
        insert_tool_call(
            &conn,
            "one:call",
            "one",
            "mcp__test__call",
            None,
            Some(&display),
            false,
            0,
        )
        .unwrap();
        let saved =
            crate::native_engine::mcp_output::McpOutput::load(&conn, "one", "one:call", &display)
                .unwrap()
                .unwrap();
        assert_eq!(saved.content, content.content);
        assert_eq!(saved.metadata, content.metadata);
        insert_tool_call(
            &conn,
            "two:call",
            "two",
            "mcp__test__call",
            None,
            Some(&display),
            false,
            0,
        )
        .unwrap();
        assert!(crate::native_engine::mcp_output::McpOutput::load(
            &conn, "two", "two:call", &display
        )
        .unwrap()
        .is_none());
        insert_tool_call(
            &conn,
            "one:call",
            "one",
            "Read",
            None,
            Some("new native output"),
            false,
            0,
        )
        .unwrap();
        assert!(crate::native_engine::mcp_output::McpOutput::load(
            &conn,
            "one",
            "one:call",
            "new native output"
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn results_survive_reload_before_turn_completion_and_updates_do_not_duplicate() {
        for mode in ["chat", "work", "study", "code"] {
            verify_stream_tool_persistence(mode);
        }
    }

    fn verify_stream_tool_persistence(mode: &str) {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::db::schema::SCHEMA_SQL).unwrap();
        crate::db::conversation_repo::migrate_workspace_mode(&conn).unwrap();
        crate::db::conversation_repo::insert_conversation(
            &conn, "conv", None, None, None, None, None, false, false, false, "now", "now", 0,
        )
        .unwrap();
        crate::db::conversation_repo::set_workspace_mode(&conn, "conv", mode).unwrap();
        let mut tool = ToolCallRow {
            id: "assistant:call-1".into(),
            message_id: "assistant".into(),
            name: "Read".into(),
            input: Some("{}".into()),
            output: Some("first".into()),
            is_error: false,
            sort_order: 0,
        };
        save_stream_tool_result(&conn, "conv", "assistant", "Checking", &tool).unwrap();
        assert_eq!(
            get_tool_calls_for_message(&conn, "assistant").unwrap()[0]
                .output
                .as_deref(),
            Some("first")
        );
        tool.output = Some("updated".into());
        save_stream_tool_result(&conn, "conv", "assistant", "Checking again", &tool).unwrap();
        let calls = get_tool_calls_for_message(&conn, "assistant").unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].output.as_deref(), Some("updated"));
        assert_eq!(
            get_messages_by_conversation(&conn, "conv").unwrap().len(),
            1
        );
        assert_eq!(
            get_message(&conn, "assistant").unwrap().unwrap().content,
            "Checking again"
        );
    }
}

/// Persist observed execution time independently of provider result payloads.
pub fn set_tool_duration(conn: &Connection, id: &str, duration_ms: u64) -> Result<()> {
    conn.execute("INSERT INTO tool_call_timings(tool_call_id,duration_ms) VALUES (?1,?2) ON CONFLICT(tool_call_id) DO UPDATE SET duration_ms=excluded.duration_ms", params![id, duration_ms.min(i64::MAX as u64) as i64])?;
    Ok(())
}
pub fn tool_duration(conn: &Connection, id: &str) -> Result<Option<u64>> {
    use rusqlite::OptionalExtension;
    Ok(conn
        .query_row(
            "SELECT duration_ms FROM tool_call_timings WHERE tool_call_id=?1",
            params![id],
            |row| row.get::<_, u64>(0),
        )
        .optional()?)
}

#[cfg(test)]
mod duration_tests {
    use super::*;
    #[test]
    fn duration_roundtrips_and_legacy_rows_stay_unknown() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE tool_call_timings(tool_call_id TEXT PRIMARY KEY, duration_ms INTEGER NOT NULL)").unwrap();
        assert_eq!(tool_duration(&conn, "legacy").unwrap(), None);
        set_tool_duration(&conn, "call", 2050).unwrap();
        assert_eq!(tool_duration(&conn, "call").unwrap(), Some(2050));
        set_tool_duration(&conn, "zero", 0).unwrap();
        assert_eq!(tool_duration(&conn, "zero").unwrap(), Some(0));
    }
}

#[cfg(test)]
mod trajectory_tests {
    use super::*;
    use serde_json::json;

    fn fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::db::schema::SCHEMA_SQL).unwrap();
        crate::db::conversation_repo::insert_conversation(
            &conn, "conv", None, None, None, None, None, false, false, false, "now", "now", 0,
        )
        .unwrap();
        insert_message(
            &conn,
            "assistant",
            "conv",
            "assistant",
            "",
            None,
            "now",
            false,
            0,
        )
        .unwrap();
        conn
    }

    fn snapshots(count: usize, prompt: &str) -> serde_json::Value {
        let tools = json!([{"name": "Read", "description": "reads a file"}]);
        json!((0..count)
            .map(|index| json!({
                "id": format!("request-{index}"),
                "systemPrompt": prompt,
                "tools": tools,
                "usage": {"input_tokens": index},
            }))
            .collect::<Vec<_>>())
    }

    #[test]
    fn identical_headers_are_stored_once() {
        let conn = fixture();
        let prompt = "You are a careful assistant. ".repeat(200);
        let payload = snapshots(297, &prompt);
        save_request_snapshots(&conn, "assistant", &payload).unwrap();
        let blobs: i64 = conn
            .query_row("SELECT COUNT(*) FROM request_blobs", [], |row| row.get(0))
            .unwrap();
        assert_eq!(blobs, 2, "one blob per distinct header");
        let raw: String = conn
            .query_row(
                "SELECT requests_json FROM message_trajectory WHERE message_id='assistant'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!raw.contains("You are a careful assistant"), "{raw}");
        assert!(raw.contains("systemPromptRef"), "{raw}");
        assert!(raw.contains("toolsRef"), "{raw}");
        assert!(
            raw.len() < serde_json::to_string(&payload).unwrap().len() / 20,
            "snapshots should shrink far below the inline form"
        );
    }

    #[test]
    fn round_trip_restores_every_field() {
        let conn = fixture();
        let payload = snapshots(3, "prompt");
        save_request_snapshots(&conn, "assistant", &payload).unwrap();
        let loaded = get_request_snapshots(&conn, "assistant").unwrap();
        assert_eq!(loaded, payload);
        assert_eq!(loaded[1]["tools"], payload[1]["tools"]);
        assert_eq!(loaded[2]["usage"]["input_tokens"], 2);
    }

    #[test]
    fn legacy_rows_still_read() {
        let conn = fixture();
        insert_message(
            &conn,
            "legacy",
            "conv",
            "assistant",
            "",
            None,
            "now",
            false,
            0,
        )
        .unwrap();
        conn.execute(
            "INSERT INTO message_trajectory(message_id, requests_json) VALUES (?1, ?2)",
            params![
                "legacy",
                r#"[{"id":"old","systemPrompt":"inline prompt","tools":[]}]"#
            ],
        )
        .unwrap();
        let loaded = get_request_snapshots(&conn, "legacy").unwrap();
        assert_eq!(loaded[0]["systemPrompt"], "inline prompt");
        assert_eq!(loaded[0]["tools"], json!([]));
    }

    #[test]
    fn a_missing_blob_is_reported_not_invented() {
        let conn = fixture();
        save_request_snapshots(&conn, "assistant", &snapshots(1, "prompt")).unwrap();
        conn.execute("DELETE FROM request_blobs", []).unwrap();
        let loaded = get_request_snapshots(&conn, "assistant").unwrap();
        assert!(loaded[0].get("systemPrompt").is_none(), "{loaded}");
        assert!(loaded[0]["systemPromptRef"].is_string(), "{loaded}");
        assert!(loaded[0].get("tools").is_none(), "{loaded}");
    }

    #[test]
    fn saving_does_not_mutate_the_caller_payload() {
        let conn = fixture();
        let payload = snapshots(2, "original prompt");
        save_request_snapshots(&conn, "assistant", &payload).unwrap();
        assert_eq!(payload[0]["systemPrompt"], "original prompt");
        assert!(payload[0].get("systemPromptRef").is_none());
    }
}
