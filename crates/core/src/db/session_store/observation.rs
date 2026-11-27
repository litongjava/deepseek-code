//! Observer cursors and liveness probes follow the incarnation of the selected turn.
use super::{owned::Backend, SessionRepository};
use crate::db::{
    message_repo::{self, MessageRow, ToolCallRow},
    turn_repo::{self, TurnRecord},
};
use crate::services::session_messages::{self, ExecutionLease};
use anyhow::{ensure, Context, Result};
use std::path::Path;

impl SessionRepository<'_> {
    pub fn observation_identity(&self) -> &str {
        match &self.0 {
            Backend::Admitted(s) => s.incarnation(),
            Backend::Shared(_, id) => id,
        }
    }
    pub fn execution_active(&self, root: &Path) -> Result<bool> {
        match &self.0 {
            Backend::Shared(_, id) => session_messages::execution_active(root, id),
            Backend::Admitted(s) => {
                ensure!(
                    root.canonicalize()? == s.stores.data_dir(),
                    "Observer data root changed"
                );
                let _lock = s.stores.key_lock(&s.key, false, false)?;
                s.check_directory()?;
                session_messages::execution_active(root, s.incarnation())
            }
        }
    }
    pub fn running_turn(&self) -> Result<Option<TurnRecord>> {
        self.read(|c, id| Ok(turn_repo::running_turn(c, id)?))
    }
    pub fn latest_turn(&self) -> Result<Option<TurnRecord>> {
        self.read(|c, id| Ok(turn_repo::latest_turn(c, id)?))
    }
    pub fn observed_turn(&self, turn: &str) -> Result<Option<TurnRecord>> {
        self.read(|c, id| {
            if !c
                .prepare_cached("SELECT 1 FROM execution_turns WHERE conversation_id=?1 AND id=?2")?
                .exists([id, turn])?
            {
                return Ok(None);
            }
            Ok(turn_repo::get_turn(c, turn)?)
        })
    }
    pub fn observed_segments(
        &self,
        turn: &str,
    ) -> Result<Vec<(MessageRow, Vec<(ToolCallRow, usize)>)>> {
        self.read(|c, id| {
            // Session reads already own a pinned snapshot.
            let snapshot = if c.is_autocommit() {
                Some(c.unchecked_transaction()?)
            } else {
                None
            };
            let c = snapshot.as_deref().unwrap_or(c);
            // The same order `repl::conversation` numbers saved tools with, so a
            // live observer keeps the transcript's `#N` identities.
            let mut numbers = std::collections::BTreeMap::new();
            let mut query = c.prepare(
                "SELECT t.id, ROW_NUMBER() OVER (
                     ORDER BY m.sort_order ASC, m.rowid ASC, t.sort_order ASC, t.rowid ASC)
                 FROM tool_calls t JOIN messages m ON m.id=t.message_id
                 WHERE m.conversation_id=?1",
            )?;
            for row in query.query_map([id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })? {
                let (tool, number) = row?;
                numbers.insert(tool, number.max(1) as usize);
            }
            let mut result = Vec::new();
            for message in turn_repo::turn_messages(c, turn)? {
                if let Some(row) =
                    message_repo::get_message(c, &message)?.filter(|m| m.conversation_id == id)
                {
                    let tools = message_repo::get_tool_calls_for_message(c, &message)?
                        .into_iter()
                        .map(|tool| {
                            let number = numbers.get(&tool.id).copied().unwrap_or(1);
                            (tool, number)
                        })
                        .collect();
                    result.push((row, tools));
                }
            }
            Ok(result)
        })
    }
    pub fn observed_tool_duration(&self, tool: &str) -> Result<Option<u64>> {
        self.read(|c, _| message_repo::tool_duration(c, tool))
    }
    pub fn settle_observed_turn(&self, root: &Path) -> Result<Option<TurnRecord>> {
        match &self.0 {
            Backend::Admitted(s) => {
                ensure!(
                    root.canonicalize()? == s.stores.data_dir(),
                    "Observer data root changed"
                );
                let lease = s.acquire_execution()?;
                s.settle_abandoned(&lease)?;
                self.latest_turn()
            }
            Backend::Shared(db, id) => {
                let Some(_lease) = ExecutionLease::acquire(root, id)? else {
                    return Ok(None);
                };
                db.with_conn(|c| {
                    turn_repo::settle_interrupted(c, id)?;
                    Ok(turn_repo::latest_turn(c, id)?)
                })?
            }
        }
    }
}

impl SessionRepository<'_> {
    pub(crate) fn pending_tool_inputs(&self) -> Result<Vec<(String, String, Option<String>)>> {
        self.read(|c,id|{
            let mut q=c.prepare("SELECT t.id,t.name,t.input FROM tool_calls t JOIN messages m ON m.id=t.message_id WHERE m.conversation_id=?1 AND t.output IS NULL ORDER BY m.sort_order,m.rowid,t.sort_order,t.rowid")?;
            let rows=q.query_map([id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }
}

impl super::Stores {
    pub(crate) fn former_coder_incarnations(&self, task: &str, id: &str) -> Result<Vec<String>> {
        self.global().read(|c| {
            let mut q = c.prepare(
                "SELECT incarnation FROM sa_coder_history WHERE task_id=?1 AND session_id=?2",
            )?;
            let rows = q.query_map([task, id], |r| r.get::<_, String>(0))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub(crate) fn hold_stopped_execution(
        &self,
        id: &str,
        incarnation: &str,
    ) -> Result<session_messages::ExecutionLease> {
        // Verify a still-live shard. A replacement has no bearing on this lock.
        if self.published_incarnation_matches(id, incarnation)? {
            self.open_current_session(id, Some(incarnation))?;
        }
        session_messages::ExecutionLease::try_acquire(self.data_dir(), incarnation)
            .with_context(|| format!("Session {id} still owns an active execution"))?
            .context("Invalid participant incarnation")
    }
}
