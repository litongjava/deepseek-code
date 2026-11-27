//! Lock all selected member incarnations before reading any evidence or entering
//! the global decision transaction. Locks outlive both snapshots and G commit.
use super::{layout, SessionDb, Stores};
use crate::super_agent::store::{Decision, EvidenceFence, MemberEvidence, TaskDatabase};
use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, Transaction};
use std::collections::BTreeMap;

impl SessionDb {
    pub(crate) fn router_stores(&self) -> Stores {
        let mut stores = self.scoped_stores();
        stores.router = Some(self.address());
        stores
    }
}

impl TaskDatabase for Stores {
    fn router_address(&self) -> Option<&super::HistoryAddress> {
        self.router.as_ref()
    }

    const ROUTED_TASKS: &'static str = "(SELECT t.* FROM sa_tasks t
        JOIN sa_member_sessions m ON m.task_id=t.task_id AND m.role='router' AND m.session_id=t.router_session_id
        JOIN conversations c ON c.id=m.session_id AND c.incarnation=m.incarnation)";

    fn task_read<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.global().read(f)
    }

    fn task_write<T>(
        &self,
        label: &str,
        mut f: impl FnMut(&Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        self.global()
            .connection(OpenFlags::SQLITE_OPEN_READ_WRITE, |c| {
                c.pragma_update(None, "synchronous", "NORMAL")?;
                crate::db::write_tx(c, label, crate::super_agent::store::WRITE_DEADLINE, |tx| {
                    f(tx)
                })
            })
    }

    fn fence_decision(&self, selection: Decision<'_>) -> Result<Option<EvidenceFence>> {
        let (predicate,value)=match selection {
            Decision::Task(id)=>("task_id=?1",id),
            Decision::Event(id)=>("task_id IN (SELECT task_id FROM sa_events WHERE event_id=?1 AND event_type='TASK_COMPLETED' AND processed=0)",id),
            Decision::Pending(router)=>("router_session_id=?1 AND task_id IN (SELECT task_id FROM sa_events WHERE event_type='TASK_COMPLETED' AND processed=0)",router),
        };
        let table = if matches!(selection, Decision::Pending(_)) {
            Self::ROUTED_TASKS
        } else {
            "sa_tasks"
        };
        let members = self.global().read(|c| {
            let mut q = c.prepare(&format!(
                "SELECT m.session_id,m.incarnation FROM (SELECT * FROM {table} WHERE {predicate}) t
                 JOIN sa_member_sessions m ON m.task_id=t.task_id
                 AND ((m.role='coder' AND m.session_id=t.coder_session_id)
                   OR (m.role='reviewer' AND m.session_id=t.reviewer_session_id))
                 JOIN conversations c ON c.id=m.session_id AND c.incarnation=m.incarnation"
            ))?;
            let mut cursor = q.query([value])?;
            let mut members = BTreeMap::new();
            while let Some(row) = cursor.next()? {
                let id: String = row.get(0)?;
                members.insert(layout::storage_key(&id)?, (id, row.get::<_, String>(1)?));
            }
            Ok(members)
        })?;
        let mut fence = EvidenceFence::default();
        if !members.is_empty() {
            fence.locks.push(self.namespace()?);
        }
        for (key, (id, incarnation)) in members {
            let path = self.root.layout().lock_path("session").join(&key);
            let lock = std::fs::File::options()
                .read(true)
                .write(true)
                .open(&path)
                .with_context(|| format!("Opening evidence lock for member {id}"))?;
            lock.try_lock()
                .with_context(|| format!("Super Agent member {id} is busy; retry the decision"))?;
            fence.locks.push(lock);
            let member = SessionDb {
                stores: self.clone(),
                key,
                id: id.clone(),
                incarnation: incarnation.clone(),
            };
            member.check_directory()?;
            member.upgrade_locked()?;
            let (revision,nonempty)=member.connection_under_lock(OpenFlags::SQLITE_OPEN_READ_ONLY,|c| {
                Ok(c.query_row("SELECT content_revision,EXISTS(SELECT 1 FROM messages) FROM manifest WHERE singleton=1",[],|r|Ok((r.get(0)?,r.get(1)?)))?)
            })?;
            fence.members.insert(
                id,
                MemberEvidence {
                    incarnation,
                    revision,
                    nonempty,
                },
            );
        }
        Ok(Some(fence))
    }
}
