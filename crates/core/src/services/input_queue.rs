//! Messages the user sends while a turn runs. As in the reference CLI, a
//! running turn reads them after its next tool results; whatever is still
//! queued when the turn ends becomes the next prompt.
use crate::db::DbManager;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// A consumed input retains its model content and does not become an approval note.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeliveredInput {
    pub text: String,
    pub content: Value,
    pub origin: String,
}

impl DeliveredInput {
    pub fn resolve(data_dir: Option<&std::path::Path>, session: &str, text: String) -> Self {
        let notification = text.starts_with(crate::tools::background_shell::NOTIFICATION_START);
        // Internal notifications cannot name images from the user's clipboard registry.
        let content = if notification {
            None
        } else {
            data_dir.and_then(|root| content_with_images(root, session, &text))
        }
        .map(Value::Array)
        .unwrap_or_else(|| json!(text));
        Self {
            text,
            content,
            origin: if notification {
                "task_notification"
            } else {
                "user"
            }
            .into(),
        }
    }

    pub fn model_message(&self) -> Value {
        json!({"role":"user", "content":self.content, "origin":self.origin})
    }
}

pub fn migrate(conn: &rusqlite::Connection) -> Result<()> {
    conn.execute_batch("CREATE TABLE IF NOT EXISTS tool_queued_inputs (tool_call_id TEXT PRIMARY KEY REFERENCES tool_calls(id) ON DELETE CASCADE, inputs_json TEXT NOT NULL)")?;
    for (name, definition) in [
        ("request_id", "TEXT"),
        ("revision", "INTEGER NOT NULL DEFAULT 1"),
        ("mode", "TEXT NOT NULL DEFAULT 'steer'"),
        ("state", "TEXT NOT NULL DEFAULT 'queued'"),
        ("content_json", "TEXT"),
        ("admission_json", "TEXT"),
        ("handoff_id", "TEXT"),
    ] {
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_info('session_input_queue') WHERE name=?1)",
            [name],
            |row| row.get(0),
        )?;
        if !exists {
            conn.execute_batch(&format!(
                "ALTER TABLE session_input_queue ADD COLUMN {name} {definition}"
            ))?;
        }
    }
    conn.execute_batch("CREATE UNIQUE INDEX IF NOT EXISTS idx_input_queue_request ON session_input_queue(session_id,request_id);
        CREATE TABLE IF NOT EXISTS session_input_queue_control (session_id TEXT PRIMARY KEY, paused INTEGER NOT NULL DEFAULT 0);")?;
    Ok(())
}

pub fn save_delivered(
    conn: &rusqlite::Connection,
    tool: &str,
    inputs: &[DeliveredInput],
) -> Result<()> {
    conn.execute("INSERT INTO tool_queued_inputs(tool_call_id,inputs_json) VALUES(?1,?2) ON CONFLICT(tool_call_id) DO UPDATE SET inputs_json=excluded.inputs_json",
        rusqlite::params![tool, serde_json::to_string(inputs)?])?;
    Ok(())
}

pub fn load_delivered(conn: &rusqlite::Connection, tool: &str) -> Result<Vec<DeliveredInput>> {
    use rusqlite::OptionalExtension;
    let raw: Option<String> = conn
        .query_row(
            "SELECT inputs_json FROM tool_queued_inputs WHERE tool_call_id=?1",
            [tool],
            |row| row.get(0),
        )
        .optional()?;
    Ok(match raw {
        Some(raw) => serde_json::from_str(&raw)?,
        None => Vec::new(),
    })
}

pub fn push(db: &DbManager, session: &str, text: &str) -> Result<()> {
    db.with_conn(|conn| {
        conn.execute(
            "INSERT INTO session_input_queue (session_id, content, created_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![session, text, chrono::Utc::now().to_rfc3339()],
        )
    })??;
    Ok(())
}

/// Takes legacy queued messages, oldest first. New queue entries require durable handoff.
pub fn take(db: &DbManager, session: &str) -> Result<Vec<String>> {
    db.with_conn(|conn| -> Result<Vec<String>> {
        let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
        let texts = {
            let mut statement = tx.prepare("SELECT content FROM session_input_queue WHERE session_id = ?1 AND request_id IS NULL ORDER BY id")?;
            let rows = statement.query_map([session], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        tx.execute("DELETE FROM session_input_queue WHERE session_id = ?1 AND request_id IS NULL", [session])?;
        tx.commit()?;
        Ok(texts)
    })?
}

/// Takes the queued messages `wanted` selects, oldest first, and leaves
/// the rest queued.
pub fn take_where(
    db: &DbManager,
    session: &str,
    wanted: impl Fn(&str) -> bool,
) -> Result<Vec<String>> {
    db.with_conn(|conn| -> Result<Vec<String>> {
        let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
        let rows = {
            let mut statement = tx.prepare("SELECT id, content FROM session_input_queue WHERE session_id = ?1 AND request_id IS NULL ORDER BY id")?;
            let rows = statement.query_map([session], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut texts = Vec::new();
        for (id, text) in rows.into_iter().filter(|(_, text)| wanted(text)) {
            tx.execute("DELETE FROM session_input_queue WHERE id = ?1", [id])?;
            texts.push(text);
        }
        tx.commit()?;
        Ok(texts)
    })?
}

/// Steer items the host enqueued but the turn never handed to the model are
/// cancelled and returned in arrival order. A handed-off item was delivered and
/// keeps its transcript row; an item another path already cancelled stays
/// cancelled. Hosts use this to run leftovers as their next turn exactly once.
pub fn cancel_undelivered(
    db: &DbManager,
    session: &str,
    ids: &[i64],
) -> Result<Vec<QueueItem>> {
    db.with_conn(|conn| -> Result<Vec<QueueItem>> {
        let tx = crate::db::write_transaction(conn)?;
        let mut undelivered = Vec::new();
        for &id in ids {
            let Ok(item) = get_item(&tx, session, id) else {
                continue;
            };
            if !matches!(item.state, QueueState::Queued | QueueState::Claimed) {
                continue;
            }
            tx.execute(
                "UPDATE session_input_queue SET state='cancelled',revision=revision+1 WHERE id=?1",
                [id],
            )?;
            undelivered.push(QueueItem {
                state: QueueState::Cancelled,
                revision: item.revision + 1,
                ..item
            });
        }
        tx.commit()?;
        Ok(undelivered)
    })?
}

/// Whether a message `wanted` selects is queued.
pub fn any(db: &DbManager, session: &str, wanted: impl Fn(&str) -> bool) -> Result<bool> {
    db.with_conn(|conn| -> Result<bool> {
        let mut statement = conn.prepare(
            "SELECT content FROM session_input_queue WHERE session_id = ?1 AND request_id IS NULL",
        )?;
        let rows = statement.query_map([session], |row| row.get::<_, String>(0))?;
        for text in rows {
            if wanted(&text?) {
                return Ok(true);
            }
        }
        Ok(false)
    })?
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QueueMode {
    NextTurn,
    Steer,
}

impl QueueMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::NextTurn => "next_turn",
            Self::Steer => "steer",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum QueueState {
    Queued,
    Claimed,
    Handoff,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct QueueItem {
    pub id: i64,
    pub session_id: String,
    pub request_id: String,
    pub revision: i64,
    pub content: Value,
    pub mode: QueueMode,
    pub state: QueueState,
    pub handoff_id: Option<String>,
}

/// Downcast errors to this type to distinguish HTTP conflict/validation/not-found responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueError {
    NotFound,
    Conflict,
    AttachmentsReadOnly,
    InvalidContent,
    Paused,
}

impl std::fmt::Display for QueueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Queue error: {self:?}")
    }
}
impl std::error::Error for QueueError {}

const ITEM_COLUMNS: &str = "id,session_id,request_id,revision,content_json,mode,state,handoff_id";

fn read_item(row: &rusqlite::Row<'_>) -> rusqlite::Result<QueueItem> {
    let content: String = row.get(4)?;
    let mode: String = row.get(5)?;
    let state: String = row.get(6)?;
    let decode = |value: &str| {
        serde_json::from_str::<Value>(value).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e))
        })
    };
    let invalid = || rusqlite::Error::InvalidQuery;
    Ok(QueueItem {
        id: row.get(0)?,
        session_id: row.get(1)?,
        request_id: row.get(2)?,
        revision: row.get(3)?,
        content: decode(&content)?,
        mode: match mode.as_str() {
            "next_turn" => QueueMode::NextTurn,
            "steer" => QueueMode::Steer,
            _ => return Err(invalid()),
        },
        state: match state.as_str() {
            "queued" => QueueState::Queued,
            "claimed" => QueueState::Claimed,
            "handoff" => QueueState::Handoff,
            "cancelled" => QueueState::Cancelled,
            _ => return Err(invalid()),
        },
        handoff_id: row.get(7)?,
    })
}

fn get_item(conn: &rusqlite::Connection, session: &str, id: i64) -> Result<QueueItem> {
    use rusqlite::OptionalExtension;
    conn.query_row(&format!("SELECT {ITEM_COLUMNS} FROM session_input_queue WHERE session_id=?1 AND id=?2 AND request_id IS NOT NULL"),
        rusqlite::params![session,id], read_item).optional()?.ok_or_else(|| QueueError::NotFound.into())
}

fn text_only(content: &Value) -> bool {
    content.is_string()
        || content.as_array().is_some_and(|blocks| {
            blocks
                .iter()
                .all(|b| b["type"] == "text" && b["text"].is_string())
        })
}

/// Content is stored in SQLite, including inline image bytes or already durable attachment references.
/// Callers must materialize attachments before calling; transient blob URLs are rejected.
pub fn enqueue(
    db: &DbManager,
    session: &str,
    request_id: &str,
    content: Value,
    mode: QueueMode,
) -> Result<QueueItem> {
    fn transient(value: &Value) -> bool {
        match value {
            Value::String(s) => s.starts_with("blob:"),
            Value::Array(a) => a.iter().any(transient),
            Value::Object(o) => o.values().any(transient),
            _ => false,
        }
    }
    if request_id.trim().is_empty()
        || !(content.is_string() || content.is_array())
        || transient(&content)
    {
        return Err(QueueError::InvalidContent.into());
    }
    let admission = serde_json::to_string(&json!({"content":content,"mode":mode}))?;
    db.with_conn(|conn| -> Result<QueueItem> {
        use rusqlite::OptionalExtension;
        let tx = crate::db::write_transaction(conn)?;
        let existing: Option<(i64,String)> = tx.query_row(
            "SELECT id,admission_json FROM session_input_queue WHERE session_id=?1 AND request_id=?2",
            rusqlite::params![session,request_id], |r| Ok((r.get(0)?,r.get(1)?))).optional()?;
        if let Some((id, original)) = existing {
            if original != admission { return Err(QueueError::Conflict.into()); }
            return get_item(&tx, session, id);
        }
        tx.execute("INSERT INTO session_input_queue(session_id,content,created_at,request_id,content_json,admission_json,mode) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            rusqlite::params![session,content.as_str().unwrap_or(""),chrono::Utc::now().to_rfc3339(),request_id,serde_json::to_string(&content)?,admission,mode.as_str()])?;
        let item = get_item(&tx,session,tx.last_insert_rowid())?;
        tx.commit()?;
        Ok(item)
    })?
}

/// Includes tombstones and handoffs for admission reconciliation and crash recovery.
pub fn list(db: &DbManager, session: &str) -> Result<Vec<QueueItem>> {
    db.with_conn(|conn| -> Result<Vec<QueueItem>> {
        let mut stmt = conn.prepare(&format!("SELECT {ITEM_COLUMNS} FROM session_input_queue WHERE session_id=?1 AND request_id IS NOT NULL ORDER BY id"))?;
        let rows = stmt.query_map([session],read_item)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })?
}

fn mutate(
    db: &DbManager,
    session: &str,
    id: i64,
    revision: i64,
    action: &str,
    text: Option<&str>,
) -> Result<QueueItem> {
    db.with_conn(|conn| -> Result<QueueItem> {
        let tx = crate::db::write_transaction(conn)?;
        let item = get_item(&tx,session,id)?;
        if item.revision != revision || item.state != QueueState::Queued { return Err(QueueError::Conflict.into()); }
        if action == "edit" && !text_only(&item.content) { return Err(QueueError::AttachmentsReadOnly.into()); }
        if matches!(action, "claim" | "steer") && paused_conn(&tx,session)? { return Err(QueueError::Paused.into()); }
        match action {
            "edit" => { let text = text.expect("edit text"); tx.execute("UPDATE session_input_queue SET content=?1,content_json=?2,revision=revision+1 WHERE id=?3", rusqlite::params![text,serde_json::to_string(text)?,id])?; }
            "steer" => { tx.execute("UPDATE session_input_queue SET mode='steer',revision=revision+1 WHERE id=?1", [id])?; }
            "remove" => { tx.execute("UPDATE session_input_queue SET state='cancelled',revision=revision+1 WHERE id=?1", [id])?; }
            "claim" => { tx.execute("UPDATE session_input_queue SET state='claimed',revision=revision+1 WHERE id=?1", [id])?; }
            _ => unreachable!(),
        }
        let updated = get_item(&tx,session,id)?;
        tx.commit()?;
        Ok(updated)
    })?
}

pub fn edit(
    db: &DbManager,
    session: &str,
    id: i64,
    revision: i64,
    text: &str,
) -> Result<QueueItem> {
    mutate(db, session, id, revision, "edit", Some(text))
}
pub fn remove(db: &DbManager, session: &str, id: i64, revision: i64) -> Result<QueueItem> {
    mutate(db, session, id, revision, "remove", None)
}
pub fn mark_steer(db: &DbManager, session: &str, id: i64, revision: i64) -> Result<QueueItem> {
    mutate(db, session, id, revision, "steer", None)
}
pub fn claim(db: &DbManager, session: &str, id: i64, revision: i64) -> Result<QueueItem> {
    mutate(db, session, id, revision, "claim", None)
}

pub fn claim_next(db: &DbManager, session: &str, mode: QueueMode) -> Result<Option<QueueItem>> {
    db.with_conn(|conn| -> Result<Option<QueueItem>> {
        use rusqlite::OptionalExtension;
        let tx = crate::db::write_transaction(conn)?;
        if paused_conn(&tx,session)? { return Ok(None); }
        let id: Option<i64> = tx.query_row("SELECT id FROM session_input_queue WHERE session_id=?1 AND request_id IS NOT NULL AND state='queued' AND mode=?2 ORDER BY id LIMIT 1",rusqlite::params![session,mode.as_str()], |r| r.get(0)).optional()?;
        let Some(id) = id else { return Ok(None); };
        tx.execute("UPDATE session_input_queue SET state='claimed',revision=revision+1 WHERE id=?1",[id])?;
        let item = get_item(&tx,session,id)?;
        tx.commit()?;
        Ok(Some(item))
    })?
}

/// Persist transcript and handoff together. The callback must use only this transaction,
/// never external side effects or another DbManager connection. On retry after commit it
/// is not called again. Recover claimed entries under the session execution lease; handed
/// off entries are recovered from transcript, not blindly executed again. This guarantees
/// atomic local history, not exactly-once external model/tool execution.
pub fn handoff(
    db: &DbManager,
    session: &str,
    id: i64,
    revision: i64,
    persist: impl FnOnce(&rusqlite::Transaction<'_>, &QueueItem, &str) -> Result<()>,
) -> Result<QueueItem> {
    db.with_conn(|conn| -> Result<QueueItem> {
        let tx = crate::db::write_transaction(conn)?;
        let item = get_item(&tx,session,id)?;
        if item.state == QueueState::Handoff && Some(item.revision) == revision.checked_add(1) { return Ok(item); }
        if item.state != QueueState::Claimed || item.revision != revision { return Err(QueueError::Conflict.into()); }
        let handoff_id = format!("queue-input-{id}");
        persist(&tx,&item,&handoff_id)?;
        tx.execute("UPDATE session_input_queue SET state='handoff',handoff_id=?1,revision=revision+1 WHERE id=?2",rusqlite::params![handoff_id,id])?;
        let updated = get_item(&tx,session,id)?;
        tx.commit()?;
        Ok(updated)
    })?
}

fn paused_conn(conn: &rusqlite::Connection, session: &str) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT COALESCE((SELECT paused FROM session_input_queue_control WHERE session_id=?1),0)",
        [session],
        |r| r.get(0),
    )?)
}
pub fn is_paused(db: &DbManager, session: &str) -> Result<bool> {
    db.with_conn(|conn| paused_conn(conn, session))?
}
pub fn set_paused(db: &DbManager, session: &str, paused: bool) -> Result<()> {
    db.with_conn(|conn| -> Result<()> {
        conn.execute("INSERT INTO session_input_queue_control(session_id,paused) VALUES(?1,?2) ON CONFLICT(session_id) DO UPDATE SET paused=excluded.paused",rusqlite::params![session,paused])?;
        Ok(())
    })?
}

#[cfg(test)]
mod durable_tests {
    use super::*;

    fn queued(db: &DbManager, request: &str) -> QueueItem {
        enqueue(db, "a", request, json!("hello"), QueueMode::NextTurn).unwrap()
    }

    #[test]
    fn migration_reopen_dedup_and_legacy_isolation() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("db.sqlite");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch("CREATE TABLE session_input_queue(id INTEGER PRIMARY KEY AUTOINCREMENT,session_id TEXT NOT NULL,content TEXT NOT NULL,created_at TEXT NOT NULL); INSERT INTO session_input_queue(session_id,content,created_at) VALUES('a','legacy','now');").unwrap();
        drop(conn);
        let db = DbManager::new(path.clone()).unwrap();
        db.init().unwrap();
        let first = queued(&db, "r1");
        assert_eq!(first, queued(&db, "r1"));
        assert_ne!(first.id, queued(&db, "r2").id);
        assert!(enqueue(&db, "a", "r1", json!("changed"), QueueMode::NextTurn).is_err());
        assert_eq!(take(&db, "a").unwrap(), ["legacy"]);
        assert!(take_where(&db, "a", |_| true).unwrap().is_empty());
        assert!(!any(&db, "a", |_| true).unwrap());
        assert!(list(&db, "b").unwrap().is_empty());
        assert!(claim(&db, "b", first.id, first.revision).is_err());
        set_paused(&db, "a", true).unwrap();
        drop(db);
        let db = DbManager::new(path).unwrap();
        db.init().unwrap();
        assert_eq!(list(&db, "a").unwrap().len(), 2);
        assert!(is_paused(&db, "a").unwrap());
        assert!(claim_next(&db, "a", QueueMode::NextTurn).unwrap().is_none());
        assert!(mark_steer(&db, "a", first.id, first.revision).is_err());
        assert_eq!(list(&db, "a").unwrap()[0], first);
        set_paused(&db, "a", false).unwrap();
        let changed = edit(&db, "a", first.id, first.revision, "edited").unwrap();
        assert_eq!(queued(&db, "r1"), changed);
        assert!(remove(&db, "a", first.id, first.revision).is_err());
        let steered = mark_steer(&db, "a", changed.id, changed.revision).unwrap();
        assert_eq!(
            claim_next(&db, "a", QueueMode::Steer).unwrap().unwrap().id,
            steered.id
        );
    }

    #[test]
    fn undelivered_steer_items_are_cancelled_and_delivered_ones_stay() {
        let temp = tempfile::tempdir().unwrap();
        let db = DbManager::new(temp.path().join("db.sqlite")).unwrap();
        db.init().unwrap();
        let first = enqueue(&db, "a", "one", json!("one"), QueueMode::Steer).unwrap();
        let second = enqueue(&db, "a", "two", json!("two"), QueueMode::Steer).unwrap();
        let claimed = claim(&db, "a", first.id, first.revision).unwrap();
        handoff(&db, "a", claimed.id, claimed.revision, |_, _, _| Ok(())).unwrap();
        let undelivered = cancel_undelivered(&db, "a", &[first.id, second.id]).unwrap();
        assert_eq!(undelivered.len(), 1);
        assert_eq!(undelivered[0].id, second.id);
        assert_eq!(undelivered[0].state, QueueState::Cancelled);
        let rows = list(&db, "a").unwrap();
        assert_eq!(
            rows.iter().find(|row| row.id == first.id).unwrap().state,
            QueueState::Handoff
        );
        assert_eq!(
            rows.iter().find(|row| row.id == second.id).unwrap().state,
            QueueState::Cancelled
        );
        assert!(cancel_undelivered(&db, "a", &[first.id, second.id])
            .unwrap()
            .is_empty());
    }

    #[test]
    fn handoff_failure_and_recovery_do_not_duplicate_history() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("db.sqlite");
        let db = DbManager::new(path.clone()).unwrap();
        db.init().unwrap();
        db.with_conn(|c| {
            c.execute_batch("CREATE TABLE queue_test_history(id TEXT PRIMARY KEY,content TEXT);")
        })
        .unwrap()
        .unwrap();
        let item = queued(&db, "r");
        let item = claim(&db, "a", item.id, item.revision).unwrap();
        assert!(handoff(&db, "a", item.id, item.revision, |tx, _, id| {
            tx.execute("INSERT INTO queue_test_history VALUES(?1,'failed')", [id])?;
            anyhow::bail!("simulated persistence failure")
        })
        .is_err());
        drop(db);
        let db = DbManager::new(path).unwrap();
        db.init().unwrap();
        assert_eq!(list(&db, "a").unwrap()[0].state, QueueState::Claimed);
        assert!(remove(&db, "a", item.id, item.revision).is_err());
        let delivered = handoff(&db, "a", item.id, item.revision, |tx, item, id| {
            tx.execute(
                "INSERT INTO queue_test_history VALUES(?1,?2)",
                rusqlite::params![id, item.content.to_string()],
            )?;
            Ok(())
        })
        .unwrap();
        assert_eq!(delivered.state, QueueState::Handoff);
        assert_eq!(
            handoff(&db, "a", item.id, item.revision, |_, _, _| panic!(
                "duplicate history insertion"
            ))
            .unwrap(),
            delivered
        );
        let count: i64 = db
            .with_conn(|c| c.query_row("SELECT count(*) FROM queue_test_history", [], |r| r.get(0)))
            .unwrap()
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn attachments_are_read_only_and_cancelled_admissions_stay_deduplicated() {
        let temp = tempfile::tempdir().unwrap();
        let db = DbManager::new(temp.path().join("db.sqlite")).unwrap();
        db.init().unwrap();
        let content = json!([{"type":"text","text":"hello"},{"type":"image_url","image_url":{"url":"data:image/png;base64,AA=="}}]);
        let item = enqueue(&db, "a", "image", content.clone(), QueueMode::NextTurn).unwrap();
        let err = edit(&db, "a", item.id, item.revision, "changed").unwrap_err();
        assert_eq!(
            err.downcast_ref::<QueueError>(),
            Some(&QueueError::AttachmentsReadOnly)
        );
        let removed = remove(&db, "a", item.id, item.revision).unwrap();
        assert_eq!(
            enqueue(&db, "a", "image", content, QueueMode::NextTurn).unwrap(),
            removed
        );
        assert!(claim(&db, "a", removed.id, removed.revision).is_err());
        assert!(enqueue(
            &db,
            "a",
            "blob",
            json!([{"type":"image","url":"blob:temporary"}]),
            QueueMode::NextTurn
        )
        .is_err());
    }

    #[test]
    fn edit_delete_claim_races_use_separate_connections() {
        for delete in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("db.sqlite");
            let db = DbManager::new(path.clone()).unwrap();
            db.init().unwrap();
            let item = queued(&db, "r");
            let other = DbManager::new(path).unwrap();
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let worker_barrier = barrier.clone();
            let id = item.id;
            let revision = item.revision;
            let worker = std::thread::spawn(move || {
                worker_barrier.wait();
                claim(&other, "a", id, revision)
            });
            barrier.wait();
            let mutation = if delete {
                remove(&db, "a", id, revision)
            } else {
                edit(&db, "a", id, revision, "new")
            };
            let claimed = worker.join().unwrap();
            assert_ne!(mutation.is_ok(), claimed.is_ok());
            let error = mutation.err().or_else(|| claimed.err()).unwrap();
            assert_eq!(
                error.downcast_ref::<QueueError>(),
                Some(&QueueError::Conflict)
            );
        }
    }
}

fn pasted_images_file(data_dir: &std::path::Path, session: &str) -> Option<std::path::PathBuf> {
    session
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
        .then(|| {
            data_dir
                .join("pasted-images")
                .join(format!("{session}.json"))
        })
}

/// Records the images pasted in a session's frontend, so a queued message's
/// `[Image #N]` can be resolved by whichever process delivers it.
pub fn save_pasted_images(
    data_dir: &std::path::Path,
    session: &str,
    paths: &[std::path::PathBuf],
) -> Result<()> {
    let file = pasted_images_file(data_dir, session)
        .ok_or_else(|| anyhow::anyhow!("Invalid session id"))?;
    std::fs::create_dir_all(file.parent().expect("pasted-images directory"))?;
    std::fs::write(file, serde_json::to_vec(paths)?)?;
    Ok(())
}

/// The pasted images a queued message names with `[Image #N]`, in order.
pub fn pasted_images(
    data_dir: &std::path::Path,
    session: &str,
    text: &str,
) -> Vec<std::path::PathBuf> {
    let Some(file) = pasted_images_file(data_dir, session) else {
        return Vec::new();
    };
    let Ok(paths) = std::fs::read(file)
        .map_err(anyhow::Error::from)
        .and_then(|bytes| Ok(serde_json::from_slice::<Vec<std::path::PathBuf>>(&bytes)?))
    else {
        return Vec::new();
    };
    let mut found: Vec<std::path::PathBuf> = Vec::new();
    for number in text
        .split("[Image #")
        .skip(1)
        .filter_map(|rest| rest.split_once(']')?.0.parse::<usize>().ok())
    {
        if let Some(path) = number.checked_sub(1).and_then(|index| paths.get(index)) {
            if path.is_file() && !found.contains(path) {
                found.push(path.clone());
            }
        }
    }
    found
}

/// A queued message as content blocks: its text, then each image it names.
/// The image's path is left out: with it, models tend to inspect the file
/// with tools instead of looking at the image.
pub fn content_with_images(
    data_dir: &std::path::Path,
    session: &str,
    text: &str,
) -> Option<Vec<serde_json::Value>> {
    let paths = pasted_images(data_dir, session, text);
    if paths.is_empty() {
        return None;
    }
    let mut blocks = vec![serde_json::json!({"type": "text", "text": text})];
    for path in paths {
        if let Ok(block) = crate::runtime::image_block_from_file(&path) {
            blocks.push(block);
        }
    }
    (blocks.len() > 1).then_some(blocks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queued_messages_are_taken_once_in_order_per_session() {
        let temp = tempfile::tempdir().unwrap();
        let db = DbManager::new(temp.path().join("db.sqlite")).unwrap();
        db.init().unwrap();
        push(&db, "a", "first").unwrap();
        push(&db, "b", "other").unwrap();
        push(&db, "a", "second").unwrap();
        assert_eq!(take(&db, "a").unwrap(), ["first", "second"]);
        assert!(take(&db, "a").unwrap().is_empty());
        assert_eq!(take(&db, "b").unwrap(), ["other"]);
        push(&db, "a", "typed").unwrap();
        push(&db, "a", "<note>").unwrap();
        let note = |text: &str| text.starts_with('<');
        assert!(any(&db, "a", note).unwrap());
        assert_eq!(take_where(&db, "a", |text| !note(text)).unwrap(), ["typed"]);
        assert_eq!(take(&db, "a").unwrap(), ["<note>"]);
    }

    #[test]
    fn queued_messages_resolve_pasted_image_placeholders() {
        let temp = tempfile::tempdir().unwrap();
        let png = temp.path().join("shot.png");
        std::fs::write(&png, base64::Engine::decode(&base64::engine::general_purpose::STANDARD, "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aY9sAAAAASUVORK5CYII=").unwrap()).unwrap();
        save_pasted_images(
            temp.path(),
            "s-1",
            &[temp.path().join("gone.png"), png.clone()],
        )
        .unwrap();
        assert!(content_with_images(temp.path(), "s-1", "no images").is_none());
        assert!(content_with_images(temp.path(), "s-1", "missing [Image #1]").is_none());
        let blocks = content_with_images(temp.path(), "s-1", "look [Image #2]").unwrap();
        assert_eq!(blocks[0]["text"], "look [Image #2]");
        assert_eq!(blocks[1]["type"], "image");
        assert_eq!(blocks.len(), 2);
        assert!(save_pasted_images(temp.path(), "../x", &[]).is_err());
    }
}
