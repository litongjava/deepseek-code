//! The only model reader that can fall back to a complete legacy result.
use crate::native_engine::tool_loop::{project_for_model, Projected};
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;
use serde_json::Value;
use std::borrow::Cow;
use std::path::{Path, PathBuf};

pub(crate) fn data_root(conn: &Connection) -> Result<Option<PathBuf>> {
    let bytes: Vec<u8> = conn.query_row(
        "SELECT CAST(file AS BLOB) FROM pragma_database_list WHERE name='main'",
        [],
        |r| r.get(0),
    )?;
    #[cfg(unix)]
    let file = {
        use std::os::unix::ffi::OsStringExt;
        PathBuf::from(std::ffi::OsString::from_vec(bytes))
    };
    #[cfg(not(unix))]
    let file = PathBuf::from(String::from_utf8(bytes)?);
    if file.as_os_str().is_empty() {
        return Ok(None);
    }
    let parent = file.parent();
    Ok(if file.file_name() == Some("session.db".as_ref())
        && parent.and_then(Path::parent).and_then(Path::file_name) == Some("sessions".as_ref())
    {
        parent.and_then(Path::parent).and_then(Path::parent)
    } else {
        parent
    }
    .map(Path::to_owned))
}

/// The optional override is computed from the complete result, before any consumer budgets it.
pub(crate) fn result_override(
    name: &str,
    raw: &str,
    root: Option<&Path>,
    session: &str,
    id: Option<&str>,
    cwd: &Path,
) -> Option<Projected> {
    let mut view = project_for_model(name, raw, root, session)?;
    view.text
        .push_str(&crate::native_engine::tool_loop::projected_command_notes(
            name, raw, root, id, cwd,
        ));
    Some(view)
}

#[derive(Serialize)]
pub(crate) struct ModelCall {
    pub id: String,
    pub message_id: String,
    pub name: String,
    pub input: Option<String>,
    #[serde(rename = "output")]
    pub text: Option<String>,
    pub is_error: bool,
    pub sort_order: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_call_id: Option<String>,
}

pub(crate) fn calls(conn: &Connection, message: &str, session: &str) -> Result<Vec<ModelCall>> {
    read_calls(conn, message, session, false)
}

/// Typed sidecars are keyed by occurrence and original display bytes. Pass that identity
/// through replay until restore_history has reconstructed the provider blocks.
pub(crate) fn replay_calls(
    conn: &Connection,
    message: &str,
    session: &str,
) -> Result<Vec<ModelCall>> {
    read_calls(conn, message, session, true)
}

fn read_calls(
    conn: &Connection,
    message: &str,
    session: &str,
    replay: bool,
) -> Result<Vec<ModelCall>> {
    let (mut query, has_request_links) = match conn.prepare_cached(
        "SELECT t.id,t.message_id,t.name,t.input,coalesce(s.display_text,t.model_output,t.output),
                t.is_error,t.sort_order,t.model_checked=0 AND t.model_output IS NULL AND s.tool_use_id IS NULL,
                t.request_id,t.provider_call_id
         FROM tool_calls t LEFT JOIN mcp_tool_outputs s
           ON ?3 AND s.conversation_id=?2 AND s.tool_use_id=t.id AND s.display_text=t.output
         WHERE t.message_id=?1 ORDER BY t.sort_order,t.rowid",
    ) {
        Ok(stmt) => (stmt, true),
        Err(rusqlite::Error::SqliteFailure(_, Some(ref msg))) if msg.contains("no such column") => {
            let stmt = conn.prepare_cached(
                "SELECT t.id,t.message_id,t.name,t.input,coalesce(s.display_text,t.model_output,t.output),
                        t.is_error,t.sort_order,t.model_checked=0 AND t.model_output IS NULL AND s.tool_use_id IS NULL
                 FROM tool_calls t LEFT JOIN mcp_tool_outputs s
                   ON ?3 AND s.conversation_id=?2 AND s.tool_use_id=t.id AND s.display_text=t.output
                 WHERE t.message_id=?1 ORDER BY t.sort_order,t.rowid",
            )?;
            (stmt, false)
        }
        Err(err) => return Err(err.into()),
    };
    let mut rows = query.query(rusqlite::params![message, session, replay])?;
    let mut calls = Vec::new();
    while let Some(row) = rows.next()? {
        let name: String = row.get(2)?;
        let id: String = row.get(0)?;
        let mut text: Option<String> = row.get(4)?;
        if row.get::<_, bool>(7)? && crate::native_engine::tool_loop::eligible(&name) {
            if let Some(raw) = &text {
                if let Some(view) = result_override(
                    &name,
                    raw,
                    data_root(conn)?.as_deref(),
                    session,
                    Some(&id),
                    Path::new("."),
                ) {
                    text = Some(view.text);
                }
            }
        }
        let (request_id, provider_call_id) = if has_request_links {
            (row.get(8)?, row.get(9)?)
        } else {
            (None, None)
        };
        calls.push(ModelCall {
            id,
            message_id: row.get(1)?,
            name,
            input: row.get(3)?,
            text,
            is_error: row.get(5)?,
            sort_order: row.get(6)?,
            request_id,
            provider_call_id,
        });
    }
    Ok(calls)
}

/// A protocol ID is only authoritative for its own call in this session.
pub(crate) enum StoredContent {
    Missing,
    Ordinary,
    Override(Value),
}

pub(crate) fn provider_content(
    conn: &Connection,
    session: &str,
    id: &str,
    name: &str,
    text_only: bool,
) -> Result<StoredContent> {
    let mut query = conn.prepare_cached(
        "SELECT t.model_output, s.content_json, s.model_content_json,
                CASE WHEN NOT t.model_checked AND t.model_output IS NULL THEN t.output END
         FROM tool_calls t JOIN messages m ON m.id=t.message_id
         LEFT JOIN mcp_tool_outputs s ON s.conversation_id=m.conversation_id AND s.tool_use_id=t.id AND s.display_text=t.output
         WHERE t.id=?1 AND t.name=?2 AND m.conversation_id=?3 AND t.output IS NOT NULL")?;
    let mut rows = query.query(rusqlite::params![id, name, session])?;
    let Some(row) = rows.next()? else {
        return Ok(StoredContent::Missing);
    };
    if let Some(raw) = row.get_ref(1)?.as_str_or_null()? {
        let model = match row.get::<_, Option<String>>(2)? {
            Some(json) => Some(serde_json::from_str(&json)?),
            None => crate::native_engine::mcp_output::sidecar_override(conn, session, name, raw)?,
        };
        return Ok(model.map_or(StoredContent::Ordinary, |content| {
            StoredContent::Override(if text_only {
                Value::String(crate::native_engine::mcp_output::openai_text(&content))
            } else {
                content
            })
        }));
    }
    if let Some(text) = row.get::<_, Option<String>>(0)? {
        return Ok(StoredContent::Override(Value::String(text)));
    }
    if let Some(raw) = row.get_ref(3)?.as_str_or_null()? {
        return Ok(result_override(
            name,
            raw,
            data_root(conn)?.as_deref(),
            session,
            Some(id),
            Path::new("."),
        )
        .map_or(StoredContent::Ordinary, |view| {
            StoredContent::Override(Value::String(view.text))
        }));
    }
    Ok(StoredContent::Ordinary)
}

pub(crate) fn notification(
    conn: &Connection,
    session: &str,
    role: &str,
    raw: &str,
) -> Result<Option<Projected>> {
    if role != "user" || !crate::native_engine::tool_loop::monitor_notification(raw) {
        return Ok(None);
    }
    Ok(project_for_model(
        "Monitor",
        raw,
        data_root(conn)?.as_deref(),
        session,
    ))
}

pub(crate) fn message_text(
    conn: &Connection,
    row: &crate::db::message_repo::MessageRow,
) -> Result<String> {
    if row.role == "user" {
        let text: Option<String> = conn
            .query_row(
                "SELECT model_content FROM messages WHERE id=?",
                [&row.id],
                |r| r.get(0),
            )
            .optional()?
            .flatten();
        if let Some(text) = text {
            return Ok(text);
        }
        if let Some(view) = notification(conn, &row.conversation_id, &row.role, &row.content)? {
            return Ok(view.text);
        }
    }
    Ok(row.content.clone())
}

pub(crate) fn search_text<'a>(name: &str, text: &'a str) -> Option<Cow<'a, str>> {
    let Some(name) = super::context_search_repo::projection_name(name) else {
        return Some(Cow::Borrowed(text));
    };
    let prefix = text.strip_prefix("Error: ").unwrap_or(text);
    if prefix.trim_start().starts_with('{') {
        let parsed = serde_json::Deserializer::from_str(prefix)
            .into_iter::<Value>()
            .next();
        match parsed {
            Some(Ok(value)) if name == "Read" && value.get("image").is_some() => {
                return value["content"]
                    .as_str()
                    .map(|display| Cow::Owned(display.to_owned()));
            }
            Some(Ok(_)) => {}
            _ => return None,
        }
    }
    Some(Cow::Owned(
        crate::native_engine::tool_loop::model_tool_output(name, text),
    ))
}

pub(crate) fn legacy_search(
    conn: &Connection,
    session: &str,
    name: &str,
    id: &str,
    notification: bool,
) -> Result<Option<Projected>> {
    let query = if notification {
        "SELECT CAST(content AS BLOB) FROM messages WHERE id=?"
    } else {
        "SELECT CAST(output AS BLOB) FROM tool_calls WHERE id=?"
    };
    let bytes: Vec<u8> = conn.query_row(query, [id], |r| r.get(0))?;
    let raw = String::from_utf8_lossy(&bytes);
    if notification && !crate::native_engine::tool_loop::monitor_notification(&raw) {
        return Ok(None);
    }
    Ok(result_override(
        name,
        &raw,
        data_root(conn)?.as_deref(),
        session,
        Some(id),
        Path::new("."),
    ))
}

pub(crate) fn messages(
    conn: &Connection,
    session: &str,
) -> Result<Vec<crate::db::message_repo::MessageRow>> {
    read_messages(conn, session, |row, _| row)
}

pub(crate) fn read_messages<T>(
    conn: &Connection,
    session: &str,
    mut select: impl FnMut(super::message_repo::MessageRow, bool) -> T,
) -> Result<Vec<T>> {
    let mut query = conn.prepare_cached(
        "SELECT id,conversation_id,role,coalesce(model_content,content),thinking,created_at,
                is_compact_boundary,sort_order,model_content IS NULL
         FROM messages WHERE conversation_id=? ORDER BY sort_order,rowid",
    )?;
    let mut rows = query.query([session])?;
    let mut messages = Vec::new();
    while let Some(row) = rows.next()? {
        let mut message = crate::db::message_repo::row_to_message(row)?;
        let mut collapsed = !row.get::<_, bool>(8)?;
        if !collapsed {
            if let Some(view) = notification(conn, session, &message.role, &message.content)? {
                message.content = view.text;
                collapsed = true;
            }
        }
        messages.push(select(message, collapsed));
    }
    Ok(messages)
}
