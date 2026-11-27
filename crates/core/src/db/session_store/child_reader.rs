//! A parent transcript records the exact child incarnation that produced each attempt.
use super::{SessionDb, Stores};
use crate::{
    db::DbManager,
    services::subagents::{self, Invocation},
};
use anyhow::{ensure, Result};
use serde_json::{json, Value};

fn page(
    invocation: &Invocation,
    messages: Vec<Value>,
    status: &str,
    parent_exists: bool,
    offset: usize,
    limit: usize,
) -> Value {
    let mut result = json!({"invocation":invocation,"messages":messages,"status":status,"parent_exists":parent_exists});
    let cursor = subagents::snapshot_cursor(&result);
    let count = messages.len();
    let end = offset.saturating_add(limit).min(count);
    result["messages"] = json!(messages
        .into_iter()
        .skip(offset)
        .take(limit)
        .collect::<Vec<_>>());
    result["next_offset"] = if end < count { json!(end) } else { Value::Null };
    result["cursor"] = json!(cursor);
    result
}

fn legacy_snapshot(
    conn: &rusqlite::Connection,
    root: &std::path::Path,
    parent: &str,
    child: &str,
    offset: usize,
    limit: usize,
) -> anyhow::Result<Option<Value>> {
    let tx = conn.unchecked_transaction()?;
    let Some(mut invocation) = subagents::list_invocations(&tx, parent, None)?
        .into_iter()
        .rev()
        .find(|invocation| invocation.child_session_id == child)
    else {
        return Ok(None);
    };
    let parent_exists = crate::db::conversation_repo::get_conversation(&tx, parent)?.is_some();
    let child_exists = crate::db::conversation_repo::get_conversation(&tx, child)?.is_some();
    let status = if child_exists {
        subagents::authoritative_status(&tx, root, &invocation)?
    } else {
        "missing".into()
    };
    invocation.status = status.clone();
    let messages = crate::db::transcript::enriched_messages(&tx, child)?;
    Ok(Some(page(
        &invocation,
        messages,
        &status,
        parent_exists,
        offset,
        limit,
    )))
}

impl DbManager {
    pub fn invocation_list(&self, parent: &str, tool: Option<&str>) -> Result<Value> {
        self.with_conn(|c| {
            let tx = c.unchecked_transaction()?;
            let mut invocations = subagents::list_invocations(&tx, parent, None)?;
            let mut seen = std::collections::HashSet::new();
            for row in invocations.iter_mut().rev() {
                if seen.insert(row.child_session_id.clone()) {
                    row.status = subagents::authoritative_status(&tx, self.data_dir(), row)?;
                }
            }
            invocations.retain(|r| tool.is_none_or(|t| r.tool_use_id == t));
            Ok(json!({"invocations":invocations}))
        })?
    }
    pub fn invocation_page(
        &self,
        parent: &str,
        child: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Option<Value>> {
        self.with_conn(|c| legacy_snapshot(c, self.data_dir(), parent, child, offset, limit))?
    }
}

impl SessionDb {
    fn saved_invocations(&self) -> Result<Vec<(String, Invocation)>> {
        self.read_invocations("SELECT attempt_key,child_incarnation,invocation FROM child_invocations ORDER BY rowid")
    }

    fn read_invocations(&self, query: &str) -> Result<Vec<(String, Invocation)>> {
        self.read(|r|{
            let mut q=r.0.prepare(query)?;
            let mut rows=q.query([])?;
            let mut out=Vec::new();
            while let Some(row)=rows.next()? {
                let inv:Invocation=serde_json::from_str(&row.get::<_,String>(2)?)?;
                ensure!(inv.parent_session_id==self.id && inv.key()==row.get::<_,String>(0)?,"Invalid persisted subagent association");
                out.push((row.get(1)?,inv));
            }
            Ok(out)
        })
    }

    /// Foreground children have no supervisor job. Resolve the latest attempt
    /// per incarnation and confirm its running progress against the execution lease.
    pub fn running_foreground_children(&self) -> Result<Vec<(super::HistoryAddress, String)>> {
        let candidates = self.read_invocations(
            "SELECT attempt_key,child_incarnation,invocation FROM child_invocations
             WHERE rowid IN (SELECT max(rowid) FROM child_invocations GROUP BY child_incarnation)
               AND json_extract(invocation,'$.mode')='foreground'
               AND json_extract(invocation,'$.status') IN ('starting','running') ORDER BY rowid",
        )?;
        let mut running = Vec::new();
        for (inc, inv) in candidates {
            let Some(child) = self.stores.selected_child(&inv.child_session_id, &inc)? else {
                continue;
            };
            let Some(progress) = child.read(|r| r.setting(super::settings::CHILD_PROGRESS))? else {
                continue;
            };
            let progress = Value::Object(progress);
            if subagents::status_with_progress(
                self.stores.data_dir(),
                &inv,
                Some(&progress),
                self.incarnation(),
                child.incarnation(),
            )? == "running"
            {
                let kind = progress["agent_type"].as_str().unwrap_or("general");
                running.push((child.address(), kind.to_owned()));
            }
        }
        Ok(running)
    }
}

impl Stores {
    pub(crate) fn selected_child(&self, id: &str, inc: &str) -> Result<Option<SessionDb>> {
        self.global()
            .read_directory_row(id, |c| super::directory::one(c, id))?
            .filter(|entry| entry.incarnation == inc)
            .map(|entry| self.open_listed(&entry))
            .transpose()
    }

    pub(crate) fn recorded_assistant_report(
        &self,
        id: &str,
        incarnation: &str,
    ) -> Result<Option<String>> {
        let read = || -> Result<Option<String>> {
            let Some(shard) = self.selected_child(id, incarnation)? else {
                return Ok(None);
            };
            shard.read(|r| r.last_assistant_report())
        };
        match read() {
            Ok(report) => Ok(report),
            Err(error) => {
                // Retirement can win between directory selection and the shard read.
                // Only a confirmed directory change permits the saved-result fallback.
                let current = self
                    .global()
                    .read_directory_row(id, |c| super::directory::one(c, id))?;
                if current.is_some_and(|entry| entry.incarnation == incarnation) {
                    Err(error)
                } else {
                    Ok(None)
                }
            }
        }
    }

    fn child_result(
        &self,
        parent: &SessionDb,
        inc: &str,
        inv: &Invocation,
        with_history: bool,
    ) -> Result<(String, Vec<Value>)> {
        let Some(child) = self.selected_child(&inv.child_session_id, inc)? else {
            return Ok(("missing".into(), Vec::new()));
        };
        let (progress, messages) = child.read(|r| {
            let progress = if matches!(inv.status.as_str(), "completed" | "failed" | "cancelled")
                || inv.mode == "background"
            {
                None
            } else {
                r.setting(super::settings::CHILD_PROGRESS)?
                    .map(Value::Object)
            };
            let messages = if with_history {
                r.enriched_messages()?
            } else {
                Vec::new()
            };
            Ok((progress, messages))
        })?;
        let status = subagents::status_with_progress(
            self.data_dir(),
            inv,
            progress.as_ref(),
            parent.incarnation(),
            child.incarnation(),
        )?;
        Ok((status, messages))
    }
    pub fn invocation_list(&self, id: &str, tool: Option<&str>) -> Result<Value> {
        let Some(entry) = self
            .global()
            .read_directory_row(id, |c| super::directory::one(c, id))?
        else {
            return Ok(json!({"invocations":[]}));
        };
        let parent = self.open_listed(&entry)?;
        let mut rows = parent.saved_invocations()?;
        let mut seen = std::collections::HashSet::new();
        for (inc, inv) in rows.iter_mut().rev() {
            if seen.insert(inc.clone()) && tool.is_none_or(|t| inv.tool_use_id == t) {
                inv.status = self.child_result(&parent, inc, inv, false)?.0;
            }
        }
        parent.check_directory()?;
        let invocations: Vec<_> = rows
            .into_iter()
            .map(|(_, r)| r)
            .filter(|r| tool.is_none_or(|t| r.tool_use_id == t))
            .collect();
        Ok(json!({"invocations":invocations}))
    }

    pub fn invocation_page(
        &self,
        id: &str,
        child: &str,
        offset: usize,
        limit: usize,
    ) -> Result<Option<Value>> {
        let Some(entry) = self
            .global()
            .read_directory_row(id, |c| super::directory::one(c, id))?
        else {
            return Ok(None);
        };
        let parent = self.open_listed(&entry)?;
        let Some((inc, mut inv)) = parent
            .saved_invocations()?
            .into_iter()
            .rev()
            .find(|(_, r)| r.child_session_id == child)
        else {
            return Ok(None);
        };
        let (status, rows) = self.child_result(&parent, &inc, &inv, true)?;
        inv.status = status.clone();
        parent.check_directory()?;
        Ok(Some(page(&inv, rows, &status, true, offset, limit)))
    }
}

impl super::ExecutorHistory {
    pub(crate) fn review_children(&self) -> Result<Vec<(Invocation, Option<Self>)>> {
        match self {
            Self::Session(s) => s
                .saved_invocations()?
                .into_iter()
                .map(|(inc, inv)| {
                    let handle = s
                        .stores
                        .selected_child(&inv.child_session_id, &inc)?
                        .map(Self::Session);
                    Ok((inv, handle))
                })
                .collect(),
            Self::Shared(db, id) => {
                let rows = db.with_conn(|c| subagents::list_invocations(c, id, None))??;
                rows.into_iter()
                    .map(|inv| {
                        let handle = db.executor_history(&inv.child_session_id)?;
                        Ok((inv, Some(handle)))
                    })
                    .collect()
            }
        }
    }
    pub(crate) fn review_metadata(&self) -> Result<crate::db::conversation_repo::ConversationRow> {
        use anyhow::Context;
        match self {
            Self::Session(s) => {
                let _locks = s.stores.key_lock(&s.key, false, false)?;
                s.check_directory()?;
                s.stores.global().read(|c| {
                    Ok(super::directory::one(c, s.id())?
                        .context("Session not found")?
                        .conversation)
                })
            }
            Self::Shared(db, id) => db.conversation_metadata(id)?.context("Session not found"),
        }
    }
    pub(crate) fn review_counts(&self) -> Result<(i64, i64)> {
        self.read(|c,id|{
            let counts=c.query_row("SELECT (SELECT count(*) FROM messages WHERE conversation_id=?1),
                (SELECT count(*) FROM tool_calls t JOIN messages m ON m.id=t.message_id WHERE m.conversation_id=?1)",[id],|r|Ok((r.get(0)?,r.get(1)?)))?;
            Ok(counts)
        })
    }
    pub(crate) fn review_messages(
        &self,
        cursor: u64,
        limit: usize,
    ) -> Result<
        Vec<(
            crate::db::message_repo::MessageRow,
            Vec<crate::db::model_results::ModelCall>,
        )>,
    > {
        use crate::db::message_repo;
        self.read(|conn, session| {
            let snapshot = if conn.is_autocommit() { Some(conn.unchecked_transaction()?) } else { None };
            let conn = snapshot.as_deref().unwrap_or(conn);
            let mut query = conn.prepare(
                "SELECT id,conversation_id,role,content,thinking,created_at,is_compact_boundary,sort_order FROM messages
                 WHERE conversation_id=?1 AND sort_order>=?2 ORDER BY sort_order,rowid LIMIT ?3")?;
            let mut rows = query.query(rusqlite::params![session, cursor, limit])?;
            let mut found = Vec::new();
            while let Some(row) = rows.next()? {
                let mut message = message_repo::row_to_message(row)?;
                message.content = crate::db::model_results::message_text(conn, &message)?;
                let calls = crate::db::model_results::calls(conn, &message.id, session)?;
                found.push((message, calls));
            }
            Ok(found)
        })
    }
}
impl Stores {
    pub(crate) fn review_coder(
        &self,
        task: &str,
        id: &str,
    ) -> Result<Option<super::ExecutorHistory>> {
        use rusqlite::OptionalExtension;
        let inc=self.global().read(|c|Ok(c.query_row("SELECT c.incarnation FROM sa_coder_history h JOIN conversations c ON c.id=h.session_id AND c.incarnation=h.incarnation WHERE h.task_id=?1 AND c.id=?2",[task,id],|r|r.get::<_,String>(0)).optional()?))?;
        Ok(match inc {
            Some(inc) => self
                .selected_child(id, &inc)?
                .map(super::ExecutorHistory::Session),
            None => None,
        })
    }
}
impl DbManager {
    pub(crate) fn review_coder(
        self: &std::sync::Arc<Self>,
        _task: &str,
        id: &str,
    ) -> Result<Option<super::ExecutorHistory>> {
        self.executor_history(id).map(Some)
    }
}
