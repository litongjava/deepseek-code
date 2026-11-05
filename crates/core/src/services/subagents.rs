//! Durable invocation identity and read-only child execution state.
use super::{background_sessions, ServiceContext};
use anyhow::{ensure, Result};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::hash::{Hash, Hasher};
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Invocation {
    pub parent_session_id: String,
    pub parent_message_id: String,
    pub tool_use_id: String,
    pub child_session_id: String,
    pub attempt_id: String,
    pub mode: String,
    pub status: String,
    pub description: String,
}

impl Invocation {
    pub fn key(&self) -> String {
        format!(
            "subagent_invocation:{}:{}:{}",
            self.parent_session_id, self.tool_use_id, self.attempt_id
        )
    }
}

pub fn save_invocation(context: &ServiceContext, invocation: &Invocation) -> Result<()> {
    ensure!(
        !invocation.parent_session_id.is_empty()
            && !invocation.parent_message_id.is_empty()
            && !invocation.tool_use_id.is_empty()
            && !invocation.child_session_id.is_empty()
            && !invocation.attempt_id.is_empty(),
        "Subagent invocation identity is incomplete"
    );
    context.save_setting(&invocation.key(), invocation)
}

/// Insertion order preserves distinct attempts even when a child is resumed.
/// Verify both the key and payload; never infer an association from tool prose.
pub fn list_invocations(
    conn: &Connection,
    parent: &str,
    tool: Option<&str>,
) -> Result<Vec<Invocation>> {
    let prefix = format!("subagent_invocation:{parent}:");
    let mut query = conn.prepare("SELECT key,value FROM peripheral_settings WHERE substr(key,1,length(?1))=?1 ORDER BY rowid")?;
    let rows = query.query_map([prefix], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut invocations = Vec::new();
    for row in rows {
        let (key, value) = row?;
        let invocation: Invocation = serde_json::from_str(&value)?;
        ensure!(
            key == invocation.key() && invocation.parent_session_id == parent,
            "Invalid persisted subagent association"
        );
        if tool.is_none_or(|tool| invocation.tool_use_id == tool) {
            invocations.push(invocation);
        }
    }
    Ok(invocations)
}

/// Only use this for the latest invocation of a child; historical attempts retain
/// their own saved status rather than borrowing a subsequent execution's state.
pub fn authoritative_status(
    conn: &Connection,
    root: &Path,
    invocation: &Invocation,
) -> Result<String> {
    if matches!(
        invocation.status.as_str(),
        "completed" | "failed" | "cancelled"
    ) {
        return Ok(invocation.status.clone());
    }
    if invocation.mode == "background" {
        return Ok(
            background_sessions::list(root, Some(&invocation.parent_session_id))?
                .into_iter()
                .rev()
                .find(|job| job.session_id == invocation.child_session_id)
                .map(|job| job.status)
                .unwrap_or_else(|| "unknown".into()),
        );
    }
    let saved: Option<String> = conn
        .query_row(
            "SELECT value FROM peripheral_settings WHERE key=?1",
            [format!("delegation.{}", invocation.child_session_id)],
            |row| row.get(0),
        )
        .optional()?;
    let saved = saved
        .map(|value| serde_json::from_str::<Value>(&value))
        .transpose()?;
    let status = saved
        .as_ref()
        .filter(|value| {
            value["parent_session_id"] == invocation.parent_session_id
                && value["session_id"] == invocation.child_session_id
        })
        .and_then(|value| value["status"].as_str())
        .unwrap_or("unknown");
    // Persisted activity is not evidence that its owner survived a host restart.
    // Absence of an OS execution lease means unknown, never inferred completion.
    if status == "running" || invocation.status == "starting" {
        let child_active =
            super::session_messages::execution_active(root, &invocation.child_session_id)?;
        let starting_owner_active = invocation.status == "starting"
            && super::session_messages::execution_active(root, &invocation.parent_session_id)?;
        if !child_active {
            return Ok(if starting_owner_active {
                "starting"
            } else {
                "unknown"
            }
            .into());
        }
    }
    // A previous execution may still have terminal state while a resume starts.
    Ok(if invocation.status == "starting" && status != "running" {
        "starting"
    } else {
        status
    }
    .to_owned())
}

/// Typed sidecars are bound to the durable tool row, session, display and error
/// state. A reused tool ID cannot attach an unrelated image to a history row.
pub fn typed_mcp_result(
    conn: &Connection,
    session: &str,
    tool: &str,
    display: Option<&str>,
    is_error: bool,
) -> Result<Option<Value>> {
    let Some(display) = display else {
        return Ok(None);
    };
    let row: Option<(String, String)> = conn.query_row(
        "SELECT content_json,metadata_json FROM mcp_tool_outputs WHERE conversation_id=?1 AND tool_use_id=?2 AND display_text=?3 AND is_error=?4",
        rusqlite::params![session, tool, display, is_error], |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional()?;
    row.map(|(content, metadata)| Ok(json!({"content":serde_json::from_str::<Value>(&content)?,"metadata":serde_json::from_str::<Value>(&metadata)?}))).transpose()
}

/// This is a snapshot fingerprint, not an event cursor. Hash the full snapshot
/// before pagination so callers can detect changes between any two pages.
pub fn snapshot_cursor(snapshot: &Value) -> String {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    snapshot.to_string().hash(&mut hash);
    format!("snapshot-v1-{:016x}", hash.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE peripheral_settings(key TEXT PRIMARY KEY,value TEXT NOT NULL); CREATE TABLE mcp_tool_outputs(conversation_id TEXT,tool_use_id TEXT,content_json TEXT,metadata_json TEXT,display_text TEXT,is_error INTEGER);").unwrap();
        conn
    }

    fn invocation(attempt: &str) -> Invocation {
        Invocation {
            parent_session_id: "parent_%".into(),
            parent_message_id: "message".into(),
            tool_use_id: "tool".into(),
            child_session_id: "child".into(),
            attempt_id: attempt.into(),
            mode: "foreground".into(),
            status: "running".into(),
            description: "same description".into(),
        }
    }

    #[test]
    fn association_is_exact_and_attempts_survive_reload() {
        let conn = fixture();
        for attempt in ["one", "two"] {
            let item = invocation(attempt);
            conn.execute(
                "INSERT INTO peripheral_settings VALUES(?1,?2)",
                [item.key(), serde_json::to_string(&item).unwrap()],
            )
            .unwrap();
        }
        assert_eq!(
            list_invocations(&conn, "parent_%", Some("tool"))
                .unwrap()
                .len(),
            2
        );
        assert!(list_invocations(&conn, "parent", None).unwrap().is_empty());
        assert!(list_invocations(&conn, "parent_%", Some("other"))
            .unwrap()
            .is_empty());
        let item = invocation("two");
        assert_eq!(
            authoritative_status(&conn, Path::new("."), &item).unwrap(),
            "unknown"
        );
        conn.execute(
            "INSERT INTO peripheral_settings VALUES('delegation.child',?1)",
            [
                json!({"session_id":"child","parent_session_id":"parent_%","status":"cancelled"})
                    .to_string(),
            ],
        )
        .unwrap();
        assert_eq!(
            authoritative_status(&conn, Path::new("."), &item).unwrap(),
            "cancelled"
        );
    }

    #[test]
    fn running_requires_live_execution_owner_and_stale_lock_is_unknown() {
        let conn = fixture();
        let root = tempfile::tempdir().unwrap();
        let mut item = invocation("attempt");
        item.parent_session_id = uuid::Uuid::new_v4().to_string();
        item.child_session_id = uuid::Uuid::new_v4().to_string();
        conn.execute(
            "INSERT INTO peripheral_settings VALUES(?1,?2)",
            [
                format!("delegation.{}", item.child_session_id),
                json!({
                    "session_id":item.child_session_id,
                    "parent_session_id":item.parent_session_id,
                    "status":"running"
                })
                .to_string(),
            ],
        )
        .unwrap();
        assert_eq!(
            authoritative_status(&conn, root.path(), &item).unwrap(),
            "unknown"
        );
        let lease = super::super::session_messages::ExecutionLease::acquire(
            root.path(),
            &item.child_session_id,
        )
        .unwrap();
        assert_eq!(
            authoritative_status(&conn, root.path(), &item).unwrap(),
            "running"
        );
        drop(lease);
        assert_eq!(
            authoritative_status(&conn, root.path(), &item).unwrap(),
            "unknown"
        );
        item.status = "starting".into();
        let parent_lease = super::super::session_messages::ExecutionLease::acquire(
            root.path(),
            &item.parent_session_id,
        )
        .unwrap();
        assert_eq!(
            authoritative_status(&conn, root.path(), &item).unwrap(),
            "starting"
        );
        drop(parent_lease);
        assert_eq!(
            authoritative_status(&conn, root.path(), &item).unwrap(),
            "unknown"
        );
        item.status = "cancelled".into();
        assert_eq!(
            authoritative_status(&conn, root.path(), &item).unwrap(),
            "cancelled"
        );
    }

    #[test]
    fn typed_sidecar_requires_full_identity_and_keeps_image_bytes() {
        let conn = fixture();
        let content = json!([{"type":"image","source":{"type":"base64","media_type":"image/png","data":"YWJj"}}]);
        conn.execute(
            "INSERT INTO mcp_tool_outputs VALUES('child','message:tool',?1,'{}','image',0)",
            [content.to_string()],
        )
        .unwrap();
        assert_eq!(
            typed_mcp_result(&conn, "child", "message:tool", Some("image"), false)
                .unwrap()
                .unwrap()["content"],
            content
        );
        for (session, tool, display, error) in [
            ("parent", "message:tool", "image", false),
            ("child", "tool", "image", false),
            ("child", "message:tool", "changed", false),
            ("child", "message:tool", "image", true),
        ] {
            assert!(typed_mcp_result(&conn, session, tool, Some(display), error)
                .unwrap()
                .is_none());
        }
    }

    #[test]
    fn cursor_changes_for_in_place_updates_not_only_message_count() {
        let first = json!({"messages":[{"id":"stable","content":"partial"}],"status":"running"});
        let mut second = first.clone();
        second["messages"][0]["content"] = json!("full text");
        assert_eq!(snapshot_cursor(&first), snapshot_cursor(&first.clone()));
        assert_ne!(snapshot_cursor(&first), snapshot_cursor(&second));
        second = first.clone();
        second["status"] = json!("completed");
        assert_ne!(snapshot_cursor(&first), snapshot_cursor(&second));
    }
}
