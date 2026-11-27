//! Short-lived repository selection for services whose public methods take a session ID.
use super::{SessionDb, Stores};
use crate::db::DbManager;
use anyhow::{ensure, Context, Result};
use rusqlite::Connection;
use std::sync::Arc;

pub trait SessionAccess {
    fn select_session<'a>(&'a self, id: &'a str) -> Result<SessionRepository<'a>>;
}
pub struct SessionRepository<'a>(pub(super) Backend<'a>);
pub(super) enum Backend<'a> {
    Admitted(SessionDb),
    Shared(&'a DbManager, &'a str),
}
impl SessionAccess for Stores {
    fn select_session<'a>(&'a self, id: &'a str) -> Result<SessionRepository<'a>> {
        Ok(SessionRepository(Backend::Admitted(self.open_session(id)?)))
    }
}
impl SessionAccess for SessionDb {
    fn select_session<'a>(&'a self, id: &'a str) -> Result<SessionRepository<'a>> {
        ensure!(
            self.id() == id,
            "Session handle does not own the requested history"
        );
        Ok(SessionRepository(Backend::Admitted(self.clone())))
    }
}
impl SessionAccess for super::ExecutorHistory {
    fn select_session<'a>(&'a self, id:&'a str) -> Result<SessionRepository<'a>> {
        ensure!(id==self.id(), "Executor history belongs to another session");
        match self {
            Self::Session(shard) => shard.select_session(id),
            Self::Shared(db, _) => db.select_session(id),
        }
    }
}
impl SessionAccess for DbManager {
    fn select_session<'a>(&'a self, id: &'a str) -> Result<SessionRepository<'a>> {
        Ok(SessionRepository(Backend::Shared(self, id)))
    }
}
impl<D: SessionAccess> SessionAccess for Arc<D> {
    fn select_session<'a>(&'a self, id: &'a str) -> Result<SessionRepository<'a>> {
        self.as_ref().select_session(id)
    }
}

impl SessionDb {
    pub fn repository(&self) -> SessionRepository<'static> {
        SessionRepository(Backend::Admitted(self.clone()))
    }
}

impl SessionRepository<'_> {
    pub(crate) fn selected_shard(&self) -> Option<&SessionDb> {
        match &self.0 {
            Backend::Admitted(shard) => Some(shard),
            Backend::Shared(_, _) => None,
        }
    }

    pub(super) fn read<T>(&self, body: impl FnOnce(&Connection, &str) -> Result<T>) -> Result<T> {
        match &self.0 {
            Backend::Admitted(s) => s.read(|r| body(r.0, s.id())),
            Backend::Shared(db, id) => db.with_conn(|c| body(c, id))?,
        }
    }
    pub(super) fn write<T>(
        &self,
        label: &str,
        mut body: impl FnMut(&Connection, &str) -> Result<T>,
    ) -> Result<T> {
        match &self.0 {
            Backend::Admitted(s) => s.write_named(label, |w| body(w.0, s.id())),
            Backend::Shared(db, id) => db.with_conn(|c| {
                crate::db::write_tx(c, label, super::WRITE_DEADLINE, |tx| body(tx, id))
            })?,
        }
    }
}

impl SessionRepository<'_> {
    pub(crate) fn saved_mode(&self) -> Result<Option<crate::permissions::PermissionMode>> {
        self.read(|c, id| {
            super::state::local(c, &format!("cli.permission-mode.{id}"))?
                .map(|s| crate::services::session_permission_mode::parse(&s))
                .transpose()
        })
    }
    pub(crate) fn saved_plan_origin(&self) -> Result<bool> {
        self.read(|c, id| {
            Ok(
                super::state::local(c, &format!("plan.return-bypass.{id}"))?.as_deref()
                    == Some("true"),
            )
        })
    }
    /// The in-memory policy changes under the manager lock only if SQLite commits.
    pub(crate) fn commit_mode(
        &self,
        manager: &crate::permissions::PermissionManager,
        mode: crate::permissions::PermissionMode,
    ) -> Result<()> {
        let label = "session permission mode";
        let deadline = crate::db::STARTUP_WRITE_DEADLINE;
        let commit =
            |tx: rusqlite::Transaction<'_>| manager.commit_mode_update(mode, || Ok(tx.commit()?));
        match &self.0 {
            Backend::Admitted(s) => {
                let _guard = s.stores.key_lock(&s.key, false, false)?;
                s.write_locked_with_commit(
                    label,
                    deadline,
                    false,
                    |w| w.set_agent_mode_from(mode, manager.get_mode(), manager.bypass_available()),
                    commit,
                )
            }
            Backend::Shared(db, id) => db.with_conn(|c| {
                crate::db::write_tx_with_commit(c, label, deadline, |tx| {
                    super::state::save_agent_mode(
                        &tx,
                        id,
                        mode,
                        manager.get_mode(),
                        manager.bypass_available(),
                    )?;
                    commit(tx)
                })
            })?,
        }
    }
}

impl SessionRepository<'_> {
    fn preference_key<T>(&self, key: &super::settings::Setting<T>) -> Result<String> {
        let name = key.name.as_ref();
        if let Backend::Shared(_, id) = &self.0 {
            let legacy = match name {
                "cli.display" | "cli.ide" | "cli.permissions" | "cli.session"
                | "cli.live-requests" | "microcompact" | "agent-description" | "fork-of" => {
                    format!("{name}.{id}")
                }
                "delegation.progress" => format!("delegation.{id}"),
                "session-start-context" | "lifecycle-instructions" => format!("{name}:{id}"),
                "lifecycle-instructions.boundary" => {
                    format!("lifecycle-instructions:{id}:boundary")
                }
                "entered-worktree" => format!("entered-worktree.{id}"),
                _ => key
                    .legacy
                    .as_ref()
                    .map(|suffix| format!("lifecycle-config:{id}:{suffix}"))
                    .context("Unsupported session preference")?,
            };
            return Ok(legacy);
        }
        Ok(name.into())
    }
    pub(crate) fn permission_rules(&self)->Result<std::collections::HashMap<String,crate::permissions::rules::PermissionAction>> {
        let key=self.preference_key(&super::settings::PERMISSIONS)?;
        let raw=self.read(|c,_|super::state::local(c,&key))?;
        Ok(raw.map(|s|serde_json::from_str(&s)).transpose().context("Stored session permission rules are invalid")?.unwrap_or_default())
    }
    pub fn preference<T: serde::de::DeserializeOwned>(
        &self,
        key: super::settings::Setting<T>,
    ) -> Result<Option<T>> {
        let name = self.preference_key(&key)?;
        let raw = self.read(|c, _| super::state::local(c, &name))?;
        Ok(raw
            .map(|v| serde_json::from_str::<Option<T>>(&v))
            .transpose()?
            .flatten())
    }
    pub fn set_preference<T: serde::Serialize>(
        &self,
        key: super::settings::Setting<T>,
        value: Option<&T>,
    ) -> Result<()> {
        let name = self.preference_key(&key)?;
        let value = value.map(serde_json::to_string).transpose()?;
        self.write("save session preference", |c, _| match &value {
            Some(value) => super::state::set_local(c, &name, value),
            None => {
                c.execute("DELETE FROM peripheral_settings WHERE key=?1", [&name])?;
                Ok(())
            }
        })
    }

    pub fn messages(&self) -> Result<Vec<crate::db::message_repo::MessageRow>> {
        self.read(crate::db::message_repo::get_messages_by_conversation)
    }

    pub fn active_history_size(&self) -> Result<(usize, usize)> {
        self.read(|c,id| {
            let mut query=c.prepare_cached(
                "WITH boundary AS (SELECT sort_order,rowid AS sequence FROM messages WHERE conversation_id=?1 AND is_compact_boundary=1 ORDER BY sort_order DESC,rowid DESC LIMIT 1)
                 SELECT length(CAST(m.content AS BLOB)),
                    (SELECT coalesce(sum(length(CAST(t.name AS BLOB))+coalesce(length(CAST(t.input AS BLOB)),0)+coalesce(length(CAST(t.output AS BLOB)),0)),0)
                     FROM tool_calls t WHERE t.message_id=m.id)
                 FROM messages m WHERE m.conversation_id=?1 AND (NOT EXISTS(SELECT 1 FROM boundary) OR
                     (m.sort_order,m.rowid)>=(SELECT sort_order,sequence FROM boundary))")?;
            let mut rows=query.query([id])?;
            let (mut count,mut size)=(0usize,0usize);
            while let Some(row)=rows.next()? {
                count+=1;
                size=size.saturating_add(row.get(0)?).saturating_add(row.get(1)?);
            }
            Ok((count,size))
        })
    }
}

impl SessionRepository<'_> {
    pub(crate) fn plan_boundary(&self) -> Result<Option<String>> {
        match &self.0 {
            Backend::Admitted(s) => s.read(|r| r.approved_plan_boundary()),
            Backend::Shared(_, _) => {
                self.read(crate::services::plan_approval::latest_boundary_conn)
            }
        }
    }

    pub(crate) fn accept_plan(
        &self,
        text: &str,
        reset: bool,
        transition: Option<(
            &crate::permissions::PermissionManager,
            crate::permissions::manager::ModeSnapshot,
            crate::permissions::PermissionMode,
        )>,
    ) -> Result<Option<String>> {
        use crate::services::{permission_authority, session_permission_mode};
        let label = "commit plan decision";
        let commit = |tx: rusqlite::Transaction<'_>| {
            if let Some((manager, expected, mode)) = transition {
                manager.commit_mode_transition(expected, mode, || Ok(tx.commit()?))
            } else {
                Ok(tx.commit()?)
            }
        };
        let policy = |c: &Connection, id: &str| -> Result<()> {
            if let Some((_, _, mode)) = transition {
                super::state::set_local(
                    c,
                    &format!("cli.permission-mode.{id}"),
                    session_permission_mode::name(mode),
                )?;
                permission_authority::record_conn(
                    c,
                    id,
                    permission_authority::Source::ApprovedPlan,
                    &serde_json::json!(text),
                )?;
            }
            Ok(())
        };
        match &self.0 {
            Backend::Admitted(s) => {
                let _locks=s.stores.key_lock(&s.key,false,false)?;
                let memory = reset.then(|| crate::services::memory_files::prompt::Admission::session(s)).transpose()?;
                s.write_locked_with_commit(label,super::WRITE_DEADLINE,false,|w|{
                    let message=memory.as_ref().map(|memory|w.plan_marker(text, memory)).transpose()?;
                    policy(w.0,w.1)?;
                    Ok(message)
                },commit)
            }
            Backend::Shared(db,id) => db.with_conn(|c|crate::db::write_tx_with_commit(c,label,super::WRITE_DEADLINE,|tx|{
                let message=if reset {
                    anyhow::ensure!(crate::db::conversation_repo::conversation_metadata(&tx,id)?.is_some(),"Plan session not found");
                    let order=tx.query_row("SELECT coalesce(max(sort_order),-1)+1 FROM messages WHERE conversation_id=?1",[id],|r|r.get(0))?;
                    let message=super::boundary::create(&tx,id,order,text,super::boundary::Kind::ApprovedPlan,super::boundary::Storage::Shared)?;
                    crate::db::conversation_repo::increment_message_count(&tx,id)?;
                    super::state::set_local(&tx,&format!("plan.context_boundary.{id}"),&serde_json::json!({"boundary_id":message}).to_string())?;
                    Some(message)
                }else{None};
                policy(&tx,id)?;
                commit(tx)?;
                Ok(message)
            }))?,
        }
    }
}
