//! Durable, host-owned timing of a complete execution turn (not a model request).
use super::DbManager;
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TurnStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnRecord {
    pub turn_id: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub elapsed_ms: Option<u64>,
    pub status: TurnStatus,
}

fn read_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<TurnRecord> {
    let status: String = row.get(4)?;
    Ok(TurnRecord {
        turn_id: row.get(0)?,
        started_at: row.get(1)?,
        ended_at: row.get(2)?,
        elapsed_ms: row.get(3)?,
        status: match status.as_str() {
            "completed" => TurnStatus::Completed,
            "failed" => TurnStatus::Failed,
            "cancelled" => TurnStatus::Cancelled,
            _ => TurnStatus::Running,
        },
    })
}

pub fn get_message_turn(
    conn: &Connection,
    message_id: &str,
) -> rusqlite::Result<Option<TurnRecord>> {
    conn.query_row(
        "SELECT t.id,t.started_at,t.ended_at,t.elapsed_ms,t.status FROM execution_turns t JOIN message_turns m ON m.turn_id=t.id WHERE m.message_id=?1",
        [message_id], read_record,
    ).optional()
}

pub fn link_message(conn: &Connection, turn_id: &str, message_id: &str) -> Result<()> {
    let inserted = conn.execute(
        "INSERT INTO message_turns(message_id,turn_id) SELECT m.id,t.id FROM messages m JOIN execution_turns t ON t.conversation_id=m.conversation_id WHERE m.id=?1 AND t.id=?2 AND m.role='assistant' ON CONFLICT(message_id) DO UPDATE SET turn_id=excluded.turn_id",
        params![message_id, turn_id],
    )?;
    anyhow::ensure!(inserted == 1, "Turn/message conversation identity mismatch");
    Ok(())
}

/// A branch gets independent identities and never inherits an active execution.
pub fn copy_message_turn(
    conn: &Connection,
    source_message: &str,
    target_message: &str,
    target_conversation: &str,
) -> Result<()> {
    let Some(source) = get_message_turn(conn, source_message)? else {
        return Ok(());
    };
    let id: Option<String> = conn
        .query_row(
            "SELECT id FROM execution_turns WHERE conversation_id=?1 AND source_turn_id=?2",
            params![target_conversation, source.turn_id],
            |row| row.get(0),
        )
        .optional()?;
    let id = match id {
        Some(id) => id,
        None => {
            let id = uuid::Uuid::new_v4().to_string();
            let status = if source.status == TurnStatus::Running {
                TurnStatus::Cancelled
            } else {
                source.status
            };
            conn.execute("INSERT INTO execution_turns(id,conversation_id,source_turn_id,started_at,ended_at,elapsed_ms,status) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                params![id,target_conversation,source.turn_id,source.started_at,source.ended_at,source.elapsed_ms,status_name(status)])?;
            id
        }
    };
    link_message(conn, &id, target_message)
}

fn status_name(status: TurnStatus) -> &'static str {
    match status {
        TurnStatus::Running => "running",
        TurnStatus::Completed => "completed",
        TurnStatus::Failed => "failed",
        TurnStatus::Cancelled => "cancelled",
    }
}

pub fn terminal_status(stop_reason: Option<&str>, failed: bool) -> TurnStatus {
    match stop_reason {
        Some("cancelled" | "interrupted") => TurnStatus::Cancelled,
        _ if failed => TurnStatus::Failed,
        Some("end_turn" | "stop" | "stop_sequence" | "tool_use") => TurnStatus::Completed,
        _ => TurnStatus::Failed,
    }
}

pub struct TurnTimer {
    db: Arc<DbManager>,
    conversation_id: String,
    started: Instant,
    record: TurnRecord,
    message_id: Option<String>,
    finished: bool,
}

impl TurnTimer {
    pub fn start(db: Arc<DbManager>, conversation_id: &str) -> Result<Self> {
        let started = Instant::now();
        let record = TurnRecord {
            turn_id: uuid::Uuid::new_v4().to_string(),
            started_at: chrono::Utc::now().to_rfc3339(),
            ended_at: None,
            elapsed_ms: None,
            status: TurnStatus::Running,
        };
        db.with_conn(|conn| conn.execute(
            "INSERT INTO execution_turns(id,conversation_id,started_at,status) VALUES(?1,?2,?3,'running')",
            params![record.turn_id, conversation_id, record.started_at],
        ))??;
        Ok(Self {
            db,
            conversation_id: conversation_id.into(),
            started,
            record,
            message_id: None,
            finished: false,
        })
    }

    pub fn record(&self) -> &TurnRecord {
        &self.record
    }
    pub fn is_finished(&self) -> bool {
        self.finished
    }
    pub fn message_id(&self) -> Option<&str> {
        self.message_id.as_deref()
    }

    /// Created after the user message, or at failure if setup never reached that point.
    pub fn ensure_segment(&mut self) -> Result<String> {
        if let Some(id) = &self.message_id {
            return Ok(id.clone());
        }
        self.new_segment()
    }

    /// Forget a segment removed by recovery; the next attempt or finish links a
    /// fresh assistant to this same execution turn.
    pub fn clear_segment(&mut self) {
        self.message_id = None;
    }

    pub fn new_segment(&mut self) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        self.db.with_conn(|conn| -> Result<()> {
            super::write_tx(conn, "start turn segment", Duration::from_secs(30), |tx| {
                tx.execute("INSERT INTO messages(id,conversation_id,role,content,created_at,sort_order) VALUES(?1,?2,'assistant','',?3,(SELECT COALESCE(MAX(sort_order),-1)+1 FROM messages WHERE conversation_id=?2))",
                    params![id,self.conversation_id,chrono::Utc::now().to_rfc3339()])?;
                link_message(tx, &self.record.turn_id, &id)?;
                tx.execute("UPDATE conversations SET message_count=(SELECT COUNT(*) FROM messages WHERE conversation_id=?1) WHERE id=?1", [&self.conversation_id])?;
                Ok(())
            })
        })??;
        self.message_id = Some(id.clone());
        Ok(id)
    }

    pub fn finish(&mut self, status: TurnStatus) -> Result<TurnRecord> {
        self.finish_at(status, Instant::now(), chrono::Utc::now().to_rfc3339())
    }

    fn finish_at(
        &mut self,
        status: TurnStatus,
        ended: Instant,
        ended_at: String,
    ) -> Result<TurnRecord> {
        if self.finished {
            return Ok(self.record.clone());
        }
        anyhow::ensure!(
            status != TurnStatus::Running,
            "Cannot finalize a running turn"
        );
        let elapsed_ms = ended
            .duration_since(self.started)
            .as_millis()
            .min(i64::MAX as u128) as u64;
        self.ensure_segment()?;
        // Finalize in the database first; memory fields change only after the
        // write commits, so a retry never re-applies half of the state.
        self.db.with_conn(|conn| {
            super::write_tx(conn, "finish turn", Duration::from_secs(30), |tx| {
                tx.execute(
                    "UPDATE execution_turns SET ended_at=?2,elapsed_ms=?3,status=?4 WHERE id=?1 AND status='running'",
                    params![self.record.turn_id,&ended_at,elapsed_ms,status_name(status)],
                )?;
                Ok(())
            })
        })??;
        self.record.ended_at = Some(ended_at);
        self.record.elapsed_ms = Some(elapsed_ms);
        self.record.status = status;
        self.finished = true;
        Ok(self.record.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn fixture() -> (tempfile::TempDir, Arc<DbManager>, String) {
        let temp = tempfile::tempdir().unwrap();
        let db = Arc::new(DbManager::new(temp.path().join("turns.sqlite")).unwrap());
        db.init().unwrap();
        let store = crate::runtime::SessionStore::new(db.clone());
        let session = store.create(temp.path(), "model", "provider").unwrap();
        store.append(&session, "user", "task").unwrap();
        (temp, db, session)
    }

    #[test]
    fn monotonic_whole_turn_is_shared_by_segments_and_finalized_once() {
        let (_temp, db, session) = fixture();
        let mut timer = TurnTimer::start(db.clone(), &session).unwrap();
        let first = timer.ensure_segment().unwrap();
        let second = timer.new_segment().unwrap();
        assert_eq!(timer.record().elapsed_ms, None);
        // Includes all requests/tool waits; wall time can go backwards independently.
        let record = timer
            .finish_at(
                TurnStatus::Completed,
                timer.started + Duration::from_millis(128_999),
                "1900-01-01T00:00:00Z".into(),
            )
            .unwrap();
        assert_eq!(record.elapsed_ms, Some(128_999));
        assert_eq!(
            timer
                .finish_at(
                    TurnStatus::Failed,
                    timer.started + Duration::from_secs(900),
                    "later".into()
                )
                .unwrap(),
            record
        );
        db.with_conn(|conn| {
            assert_eq!(
                get_message_turn(conn, &first).unwrap(),
                Some(record.clone())
            );
            assert_eq!(
                get_message_turn(conn, &second).unwrap(),
                Some(record.clone())
            );
        })
        .unwrap();
        let mut next = TurnTimer::start(db, &session).unwrap();
        assert_ne!(next.record().turn_id, record.turn_id);
        assert_eq!(
            next.finish_at(
                TurnStatus::Failed,
                next.started + Duration::from_millis(2),
                "now".into()
            )
            .unwrap()
            .elapsed_ms,
            Some(2)
        );
        let json = serde_json::to_value(record).unwrap();
        assert_eq!(json["elapsedMs"], 128_999);
        assert_eq!(json["status"], "completed");
        assert!(json.get("turn_id").is_none());
    }

    #[test]
    fn early_failure_and_dropped_future_get_durable_empty_assistants() {
        let (_temp, db, session) = fixture();
        let mut failed = TurnTimer::start(db.clone(), &session).unwrap();
        failed.finish(TurnStatus::Failed).unwrap();
        let failed_message = failed.message_id().unwrap().to_owned();
        let mut cancelled = TurnTimer::start(db.clone(), &session).unwrap();
        let cancelled_message = cancelled.ensure_segment().unwrap();
        drop(cancelled);
        db.with_conn(|conn| {
            assert_eq!(
                get_message_turn(conn, &failed_message)
                    .unwrap()
                    .unwrap()
                    .status,
                TurnStatus::Failed
            );
            let record = get_message_turn(conn, &cancelled_message).unwrap().unwrap();
            assert_eq!(record.status, TurnStatus::Cancelled);
            assert!(record.elapsed_ms.is_some());
            assert_eq!(
                super::super::message_repo::get_message(conn, &failed_message)
                    .unwrap()
                    .unwrap()
                    .content,
                ""
            );
        })
        .unwrap();
    }

    #[test]
    fn reopen_preserves_terminal_and_unknown_endpoints_without_clock_reconstruction() {
        let (temp, db, session) = fixture();
        let mut complete = TurnTimer::start(db.clone(), &session).unwrap();
        let record = complete.finish(TurnStatus::Completed).unwrap();
        let message = complete.message_id().unwrap().to_owned();
        let mut interrupted = TurnTimer::start(db.clone(), &session).unwrap();
        let unknown = interrupted.ensure_segment().unwrap();
        // Simulate process loss without running the cancellation guard.
        interrupted.finished = true;
        drop(interrupted);
        drop(complete);
        drop(db);
        let reopened = DbManager::new(temp.path().join("turns.sqlite")).unwrap();
        reopened.init().unwrap();
        reopened
            .with_conn(|conn| {
                assert_eq!(get_message_turn(conn, &message).unwrap(), Some(record));
                let record = get_message_turn(conn, &unknown).unwrap().unwrap();
                assert_eq!(record.status, TurnStatus::Running);
                assert_eq!(record.elapsed_ms, None);
                assert_eq!(record.ended_at, None);
                assert!(get_message_turn(conn, "legacy").unwrap().is_none());
            })
            .unwrap();
    }

    #[test]
    fn branches_share_only_their_own_turn_and_deletion_removes_associations() {
        let (_temp, db, session) = fixture();
        let mut timer = TurnTimer::start(db.clone(), &session).unwrap();
        let first = timer.ensure_segment().unwrap();
        let second = timer.new_segment().unwrap();
        let original = timer.finish(TurnStatus::Completed).unwrap();
        db.with_conn(|conn| {
            let branch = super::super::conversation_repo::branch_conversation(
                conn,
                &session,
                Some(&second),
                None,
            )
            .unwrap();
            let rows =
                super::super::message_repo::get_messages_by_conversation(conn, &branch).unwrap();
            let copied: Vec<_> = rows
                .iter()
                .filter_map(|row| get_message_turn(conn, &row.id).unwrap())
                .collect();
            assert_eq!(copied.len(), 2);
            assert_eq!(copied[0], copied[1]);
            assert_ne!(copied[0].turn_id, original.turn_id);
            assert_eq!(copied[0].elapsed_ms, original.elapsed_ms);
            assert!(link_message(conn, &original.turn_id, &rows[1].id).is_err());
            super::super::message_repo::delete_message(conn, &first).unwrap();
            assert!(get_message_turn(conn, &first).unwrap().is_none());
            assert!(get_message_turn(conn, &second).unwrap().is_some());
        })
        .unwrap();
    }

    #[test]
    fn running_branch_never_claims_a_live_execution_or_duration() {
        let (_temp, db, session) = fixture();
        let mut timer = TurnTimer::start(db.clone(), &session).unwrap();
        let message = timer.ensure_segment().unwrap();
        db.with_conn(|conn| {
            let branch = super::super::conversation_repo::branch_conversation(
                conn,
                &session,
                Some(&message),
                None,
            )
            .unwrap();
            let rows =
                super::super::message_repo::get_messages_by_conversation(conn, &branch).unwrap();
            let copied = get_message_turn(conn, &rows.last().unwrap().id)
                .unwrap()
                .unwrap();
            assert_eq!(copied.status, TurnStatus::Cancelled);
            assert_eq!(copied.ended_at, None);
            assert_eq!(copied.elapsed_ms, None);
        })
        .unwrap();
    }

    #[test]
    fn recovery_rebinds_deleted_segments_without_finishing_the_turn() {
        let (_temp, db, session) = fixture();
        let mut timer = TurnTimer::start(db.clone(), &session).unwrap();
        let removed = timer.ensure_segment().unwrap();
        db.with_conn(|conn| super::super::message_repo::delete_message(conn, &removed))
            .unwrap()
            .unwrap();
        timer.clear_segment();
        assert!(!timer.is_finished());
        let retry = timer.ensure_segment().unwrap();
        assert_ne!(retry, removed);
        let record = timer.finish(TurnStatus::Cancelled).unwrap();
        db.with_conn(|conn| {
            assert!(get_message_turn(conn, &removed).unwrap().is_none());
            assert_eq!(get_message_turn(conn, &retry).unwrap(), Some(record));
        })
        .unwrap();
    }

    #[test]
    fn only_explicit_success_is_completed() {
        for reason in [
            None,
            Some("channel_closed"),
            Some("panic"),
            Some("max_tokens"),
            Some("length"),
            Some("max_iterations"),
            Some("unknown"),
        ] {
            assert_eq!(terminal_status(reason, false), TurnStatus::Failed);
        }
        assert_eq!(
            terminal_status(Some("end_turn"), false),
            TurnStatus::Completed
        );
        assert_eq!(terminal_status(Some("stop"), true), TurnStatus::Failed);
        assert_eq!(
            terminal_status(Some("cancelled"), true),
            TurnStatus::Cancelled
        );
        assert_eq!(
            terminal_status(Some("interrupted"), false),
            TurnStatus::Cancelled
        );
    }
}

impl Drop for TurnTimer {
    fn drop(&mut self) {
        if !self.finished {
            // Future/task cancellation is known; process death never runs Drop and
            // leaves an unknown endpoint (no elapsed time) in the database.
            if let Err(error) = self.finish(TurnStatus::Cancelled) {
                tracing::error!("Could not finalize interrupted turn: {error:#}");
            }
        }
    }
}
