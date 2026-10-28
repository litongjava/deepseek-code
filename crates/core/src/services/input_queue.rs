//! Messages the user sends while a turn runs. As in the reference CLI, a
//! running turn reads them after its next tool results; whatever is still
//! queued when the turn ends becomes the next prompt.
use crate::db::DbManager;
use anyhow::Result;

pub fn push(db: &DbManager, session: &str, text: &str) -> Result<()> {
    db.with_conn(|conn| {
        conn.execute(
            "INSERT INTO session_input_queue (session_id, content, created_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![session, text, chrono::Utc::now().to_rfc3339()],
        )
    })??;
    Ok(())
}

/// Takes every queued message, oldest first; each is delivered exactly once.
pub fn take(db: &DbManager, session: &str) -> Result<Vec<String>> {
    db.with_conn(|conn| -> Result<Vec<String>> {
        let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
        let texts = {
            let mut statement = tx.prepare("SELECT content FROM session_input_queue WHERE session_id = ?1 ORDER BY id")?;
            let rows = statement.query_map([session], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        tx.execute("DELETE FROM session_input_queue WHERE session_id = ?1", [session])?;
        tx.commit()?;
        Ok(texts)
    })?
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
    }
}
