//! A model-requested compaction, queued and executed before the next user
//! message. The model knows when a task is finished; the automatic threshold
//! does not, so it may summarize work still in use or keep finished history.
use crate::{
    db::{message_repo, DbManager},
    tools::ToolDefinition,
};
use anyhow::{ensure, Result};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::json;

/// The stored request. Overwriting the row replaces an earlier request.
#[derive(Serialize, Deserialize)]
struct Request {
    keep: String,
}

fn key(session: &str) -> String {
    format!("compaction.request.{session}")
}

fn read(conn: &Connection, session: &str) -> Result<Option<Request>> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM peripheral_settings WHERE key=?1",
            [key(session)],
            |row| row.get(0),
        )
        .optional()?;
    raw.map(|raw| serde_json::from_str(&raw).map_err(Into::into))
        .transpose()
}

fn save(conn: &Connection, session: &str, keep: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO peripheral_settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
        [key(session), serde_json::to_string(&Request { keep: keep.into() })?],
    )?;
    Ok(())
}

pub fn definition() -> ToolDefinition {
    ToolDefinition {
        name: "CompactContext".into(),
        description: "Ask for the conversation to be compacted before the next user message, when the earlier history is no longer needed (for example a finished task before an unrelated one). `keep` says what the summary must preserve: open tasks, decisions, file paths, facts the user may refer to. It does not run now: the current turn continues with the full history. Refused when the context is still small or a compaction happened recently. Do not call it in the middle of a task.".into(),
        input_schema: json!({"type":"object","properties":{"keep":{"type":"string","minLength":1,"maxLength":2000}},"required":["keep"],"additionalProperties":false}),
    }
}

/// Queue a request for the next compaction boundary. Refused while the context
/// is under 30% of the window or directly after a compaction, so a model cannot
/// repeatedly summarize a conversation that has nothing to drop.
pub fn request(
    db: &DbManager,
    session: &str,
    keep: &str,
    used_tokens: u64,
    window: u64,
) -> Result<String> {
    ensure!(
        !keep.trim().is_empty() && keep.chars().count() <= 2000,
        "keep must contain 1 to 2000 characters"
    );
    let pct = if window == 0 {
        0
    } else {
        used_tokens.saturating_mul(100) / window
    };
    ensure!(
        window > 0 && pct >= 30,
        "Compaction not needed: the context is {pct}% of the window (at least 30% is required)."
    );
    db.with_conn(|conn| -> Result<()> {
        let rows = message_repo::get_messages_by_conversation(conn, session)?;
        if let Some(index) = rows.iter().rposition(|row| row.is_compact_boundary) {
            let after = rows[index + 1..]
                .iter()
                .filter(|row| row.role == "user")
                .count();
            ensure!(
                after >= 2,
                "Compaction not needed: the conversation was compacted recently."
            );
        }
        save(conn, session, keep)
    })??;
    Ok("Compaction queued. It runs before the next user message, using your `keep` text as the summary instructions. Continue or finish the current turn normally.".into())
}

/// Consume the queued request exactly once. The next turn owns it.
pub fn take(db: &DbManager, session: &str) -> Result<Option<String>> {
    db.with_conn(|conn| -> Result<Option<String>> {
        let tx = crate::db::write_transaction(conn)?;
        let request = read(&tx, session)?;
        tx.execute(
            "DELETE FROM peripheral_settings WHERE key=?1",
            [key(session)],
        )?;
        tx.commit()?;
        Ok(request.map(|request| request.keep))
    })?
}

/// Drop a queued request. A successful compaction already made it obsolete.
pub fn clear(db: &DbManager, session: &str) -> Result<()> {
    db.with_conn(|conn| {
        conn.execute(
            "DELETE FROM peripheral_settings WHERE key=?1",
            [key(session)],
        )
        .map(|_| ())
    })??;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        runtime::{RuntimePaths, SessionStore},
        services::compaction::{commit, prepare},
    };

    fn fixture() -> (tempfile::TempDir, SessionStore, String) {
        let temp = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
        let store = SessionStore::new(paths.open_database().unwrap());
        let id = store.create(temp.path(), "model", "provider").unwrap();
        store.append(&id, "user", "Implement task").unwrap();
        store
            .append(&id, "assistant", "Changed file, tests pending")
            .unwrap();
        (temp, store, id)
    }

    #[test]
    fn a_request_below_thirty_percent_is_refused() {
        let (_temp, store, id) = fixture();
        let error = request(&store.db, &id, "Keep the pending tests", 20, 100).unwrap_err();
        assert!(error.to_string().contains("at least 30%"), "{error}");
        assert!(take(&store.db, &id).unwrap().is_none());
    }

    #[test]
    fn a_request_is_saved_and_taken_once() {
        let (_temp, store, id) = fixture();
        let message = request(&store.db, &id, "Keep the pending tests", 50, 100).unwrap();
        assert!(message.starts_with("Compaction queued"), "{message}");
        assert_eq!(
            take(&store.db, &id).unwrap().as_deref(),
            Some("Keep the pending tests")
        );
        assert!(take(&store.db, &id).unwrap().is_none());
    }

    #[test]
    fn a_recent_compaction_refuses_a_new_request() {
        let (_temp, store, id) = fixture();
        commit(
            &store.db,
            prepare(&store.db, &id).unwrap(),
            "Task: run remaining tests",
        )
        .unwrap();
        store.append(&id, "user", "First message after compaction").unwrap();
        let error = request(&store.db, &id, "Keep the next task", 50, 100).unwrap_err();
        assert!(error.to_string().contains("compacted recently"), "{error}");
        store
            .append(&id, "user", "Second message after compaction")
            .unwrap();
        assert!(request(&store.db, &id, "Keep the next task", 50, 100).is_ok());
    }

    #[test]
    fn a_later_request_replaces_the_earlier_one() {
        let (_temp, store, id) = fixture();
        request(&store.db, &id, "Earlier keep text", 50, 100).unwrap();
        request(&store.db, &id, "Later keep text", 60, 100).unwrap();
        assert_eq!(
            take(&store.db, &id).unwrap().as_deref(),
            Some("Later keep text")
        );
    }
}
