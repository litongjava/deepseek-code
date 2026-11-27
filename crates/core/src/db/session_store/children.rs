//! Foreground children retain the chosen shard until their final progress update.
use super::{owned::Backend, ExecutorHistory, SessionRepository};
use crate::db::{message_repo, request_repo};
use anyhow::{ensure, Context, Result};
use rusqlite::params;
use serde_json::Value;

impl ExecutorHistory {
    fn child_repo(&self) -> SessionRepository<'_> {
        match self {
            Self::Session(s) => SessionRepository(Backend::Admitted(s.clone())),
            Self::Shared(db, id) => SessionRepository(Backend::Shared(db, id)),
        }
    }
    pub(crate) fn invocation_message(&self, requested: Option<&str>) -> Result<String> {
        use rusqlite::OptionalExtension;
        if let Some(id) = requested {
            return self.read(|c, session| {
                c.query_row("SELECT id FROM messages WHERE conversation_id=?1 AND id=?2 AND role='assistant'",
                    [session,id], |r| r.get(0)).optional()?.context("Invocation parent assistant segment is missing")
            });
        }
        match self {
            // Standalone executors can invoke a child before their host has allocated
            // a response segment. Give that invocation a durable owner in parent S.
            Self::Session(parent) => parent.write(|w| w.append("assistant","")),
            Self::Shared(..) => self.read(|c, session| Ok(c.query_row(
                "SELECT id FROM messages WHERE conversation_id=?1 AND role='assistant' ORDER BY sort_order DESC,rowid DESC LIMIT 1",
                [session],|r|r.get(0)).optional()?.unwrap_or_default())),
        }
    }

    pub(crate) fn save_invocation(
        &self,
        child: &ExecutorHistory,
        row: &crate::services::subagents::Invocation,
    ) -> Result<()> {
        ensure!(
            self.id() == row.parent_session_id && child.id() == row.child_session_id,
            "Child invocation owner changed"
        );
        match (self, child) {
            (Self::Session(parent), Self::Session(child)) => parent
                .write_named("save child invocation", |w| {
                    w.child_invocation(&child.address(), row)
                }),
            (Self::Shared(..), Self::Shared(..)) => {
                let raw = serde_json::to_string(row)?;
                self.child_repo().write("save child invocation", |c, _| {
                    super::state::set_local(c, &row.key(), &raw)
                })
            }
            _ => anyhow::bail!("Child uses another history store"),
        }
    }
    pub(crate) fn save_child_state(&self, value: &Value) -> Result<()> {
        let progress = value
            .as_object()
            .context("Expected a child progress record")?;
        ensure!(
            value["session_id"].as_str() == Some(self.id()),
            "Child progress belongs to another session"
        );
        self.child_repo()
            .set_preference(super::settings::CHILD_PROGRESS, Some(progress))
    }
    pub(crate) fn require_child_resume(&self, owner: &str, error: &str) -> Result<()> {
        let key = match self {
            Self::Session(_) => "delegation.progress".into(),
            Self::Shared(_, id) => format!("delegation.{id}"),
        };
        self.child_repo().write("mark child for resume",|c,_|{
            c.execute("UPDATE peripheral_settings SET value=json_set(value,'$.status','needs_resume','$.error',?3)
                WHERE key=?1 AND json_extract(value,'$.parent_session_id')=?2 AND json_extract(value,'$.status')='completed'",
                params![key,owner,error])?;
            Ok(())
        })
    }
    pub(crate) fn child_segment(&self, message: &str, text: &str, thinking: &str) -> Result<()> {
        self.child_repo().write("save child segment",|c,id|{
            let changed=c.execute("INSERT INTO messages(id,conversation_id,role,content,thinking,created_at,sort_order)
                VALUES(?1,?2,'assistant',?3,?4,?5,(SELECT coalesce(max(sort_order),-1)+1 FROM messages WHERE conversation_id=?2))
                ON CONFLICT(id) DO UPDATE SET content=excluded.content,thinking=excluded.thinking
                WHERE messages.conversation_id=excluded.conversation_id AND messages.role='assistant'",
                params![message,id,text,thinking,chrono::Utc::now().to_rfc3339()])?;
            ensure!(changed==1,"Child segment identity changed");
            Ok(())
        })
    }
    pub(crate) fn child_stream_tool(
        &self,
        message: &str,
        text: &str,
        row: &message_repo::ToolCallRow,
    ) -> Result<()> {
        ensure!(
            row.message_id == message,
            "Child tool belongs to another message"
        );
        match self {
            Self::Session(s) => s.write_named("save child tool", |w| {
                message_repo::upsert_stream_message(w.0, w.1, message, text)?;
                w.tool(row, None)
            }),
            Self::Shared(db, id) => {
                db.with_conn(|c| message_repo::save_stream_tool_result(c, id, message, text, row))?
            }
        }
    }
    pub(crate) fn child_request(&self, message: &str, snapshot: &Value) -> Result<u64> {
        self.child_repo().write("save child request",|c,id|{
            c.execute("INSERT OR IGNORE INTO messages(id,conversation_id,role,content,created_at,sort_order)
                VALUES(?1,?2,'assistant','',?3,(SELECT coalesce(max(sort_order),-1)+1 FROM messages WHERE conversation_id=?2))",
                params![message,id,chrono::Utc::now().to_rfc3339()])?;
            ensure!(c.prepare_cached("SELECT 1 FROM messages WHERE id=?1 AND conversation_id=?2 AND role='assistant'")?.exists([message,id])?,"Child request message identity changed");
            request_repo::save_request(c,message,snapshot)?;
            let mut q=c.prepare_cached("SELECT output_tokens FROM model_requests WHERE conversation_id=?1 AND output_tokens IS NOT NULL")?;
            let mut total=0u64;
            for n in q.query_map([id],|r|r.get::<_,u64>(0))? {total=total.saturating_add(n?);}
            Ok(total)
        })
    }
}

impl<D: super::SessionAccess> crate::services::ServiceContext<D> {
    pub fn child_progress(&self, id: &str) -> Result<Option<Value>> {
        self.db
            .select_session(id)?
            .preference(super::settings::CHILD_PROGRESS)
            .map(|progress| progress.map(Value::Object))
    }
    pub fn child_description(&self, id: &str) -> Result<Option<String>> {
        self.db
            .select_session(id)?
            .preference(super::settings::AGENT_DESCRIPTION)
    }
    pub(crate) fn set_child_description(&self, id: &str, description: &str) -> Result<()> {
        self.db.select_session(id)?.set_preference(
            super::settings::AGENT_DESCRIPTION,
            Some(&description.to_owned()),
        )
    }
}

impl super::Stores {
    pub(crate) fn fork_agent(&self, parent: &str, title: &str, directive: &str) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        self.open_session(parent)?
            .branch(&id, super::Branch::AgentFork { title, directive })?;
        Ok(id)
    }
    pub(crate) fn is_agent_fork(&self, session: &str) -> Result<bool> {
        self.open_session(session)?
            .read(|r| Ok(r.setting(super::settings::AGENT_FORK_PARENT)?.is_some()))
    }
}
impl crate::db::DbManager {
    pub(crate) fn fork_agent(&self, parent: &str, title: &str, directive: &str) -> Result<String> {
        let id = self.branch_conversation(parent, None, None)?;
        self.rename_session(&id, title)?;
        self.with_conn(|c| {
            crate::db::write_tx(
                c,
                "record agent fork",
                crate::db::STARTUP_WRITE_DEADLINE,
                |tx| {
                    super::state::set_local(
                        tx,
                        &format!("fork-of.{id}"),
                        &serde_json::json!({"parent":parent}).to_string(),
                    )?;
                    crate::services::permission_authority::record_conn(
                        tx,
                        &id,
                        crate::services::permission_authority::Source::User,
                        &serde_json::json!(directive),
                    )
                },
            )
        })??;
        Ok(id)
    }
    pub(crate) fn is_agent_fork(&self, session: &str) -> Result<bool> {
        self.with_conn(|c| Ok(super::state::local(c, &format!("fork-of.{session}"))?.is_some()))?
    }
}

fn owner_binding(
    c: &rusqlite::Connection,
    root: &std::path::Path,
    parent: &str,
    child: &str,
    expected: &std::path::Path,
    split: bool,
) -> Result<Value> {
    use crate::db::conversation_repo::conversation_metadata;
    use rusqlite::OptionalExtension;
    use serde_json::json;
    let p = conversation_metadata(c, parent)?.context("Owner missing")?;
    let child_row = conversation_metadata(c, child)?.context("Child missing")?;
    ensure!(
        !p.archived && !child_row.archived,
        "Owner or child archived"
    );
    let workspaces = if !split && child_row.project_id.as_deref().is_none_or(str::is_empty) {
        expected
            .parent()
            .context("Private root missing")?
            .to_owned()
    } else {
        crate::data_layout::DataLayout::from_root(&root).workspaces()
    };
    let resolve = |id: &str| -> Result<std::path::PathBuf> {
        if split {
            super::lifecycle::workspace(c, &workspaces, id)
        } else {
            Ok(crate::native_engine::workspace::resolve_workspace(c, &workspaces, id, None)?.0)
        }
    };
    let actual = resolve(child)?.canonicalize()?;
    let owner = resolve(parent)?.canonicalize()?;
    let isolated = if actual != owner && p.project_id == child_row.project_id {
        let saved: Option<(String, String)> = c
            .query_row(
                "SELECT source_path,path FROM conversation_worktrees WHERE conversation_id=?1",
                [child],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        saved.is_some_and(|(source, path)| {
            std::path::Path::new(&source).canonicalize().ok().as_ref() == Some(&owner)
                && std::path::Path::new(&path).canonicalize().ok().as_ref() == Some(&actual)
        })
    } else {
        false
    };
    ensure!(
        (actual == owner || isolated) && actual == expected.canonicalize()?,
        "Owner/child workspace no longer matches"
    );
    let mut binding = json!({"runtime":root.canonicalize()?,"parent_project":p.project_id,"parent_mode":p.workspace_mode,"child_project":child_row.project_id,"child_mode":child_row.workspace_mode,"owner_cwd":owner,"cwd":actual,"provider":child_row.provider,"model":child_row.model});
    if split {
        let (p,k):(String,String)=c.query_row("SELECT p.incarnation,c.incarnation FROM session_execution_parents e JOIN conversations p ON (p.id,p.incarnation)=(e.parent_id,e.parent_incarnation) JOIN conversations c ON c.id=e.child_id WHERE p.id=?1 AND c.id=?2",[parent,child],|r|Ok((r.get(0)?,r.get(1)?))).context("Child execution parent changed")?;
        binding["parent_incarnation"] = p.into();
        binding["child_incarnation"] = k.into();
    }
    Ok(binding)
}

impl super::Stores {
    pub(crate) fn owned_workspace_binding(
        &self,
        parent: &str,
        child: &str,
        expected: &std::path::Path,
    ) -> Result<Value> {
        self.global().read(|c| {
            let tx = c.unchecked_transaction()?;
            owner_binding(&tx, self.data_dir(), parent, child, expected, true)
        })
    }
    pub(crate) fn child_of(&self, parent: &str, child: &str) -> Result<bool> {
        self.global().read(|c|Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM session_execution_parents e JOIN conversations p ON (p.id,p.incarnation)=(e.parent_id,e.parent_incarnation) WHERE e.child_id=?1 AND p.id=?2)",[child,parent],|r|r.get(0))?))
    }
}
impl crate::db::DbManager {
    pub(crate) fn owned_workspace_binding(
        &self,
        parent: &str,
        child: &str,
        expected: &std::path::Path,
    ) -> Result<Value> {
        self.with_conn(|c| owner_binding(c, self.data_dir(), parent, child, expected, false))?
    }
    pub(crate) fn child_of(&self, parent: &str, child: &str) -> Result<bool> {
        self.with_conn(|c|{
            if crate::services::subagents::list_invocations(c,parent,None)?.iter().any(|row|row.child_session_id==child){return Ok(true);}
            Ok(c.query_row("SELECT EXISTS(SELECT 1 FROM conversation_private_workspace_bindings WHERE child_id=?1 AND parent_id=?2)",[child,parent],|r|r.get(0))?)
        })?
    }
}

impl ExecutorHistory {
    pub(crate) fn inherit_child_authority(&self, parent: &Self) -> Result<()> {
        match (self, parent) {
            (Self::Shared(db, child), Self::Shared(other, owner)) => {
                ensure!(
                    db.data_dir() == other.data_dir(),
                    "Child belongs to another store"
                );
                crate::services::permission_authority::inherit(db, owner, child)
            }
            (Self::Session(child), Self::Session(owner)) => {
                ensure!(
                    child.stores.global().store_uuid() == owner.stores.global().store_uuid()
                        && child.stores.data_dir() == owner.stores.data_dir()
                        && child.id() != owner.id(),
                    "Child belongs to another store or session"
                );
                let _locks = child
                    .stores
                    .group_access(vec![(&child.key, false, false), (&owner.key, false, false)])?;
                child.check_directory()?;
                owner.check_directory()?;
                ensure!(
                    child.stores.child_of(owner.id(), child.id())?,
                    "Child execution parent changed"
                );
                let authority=owner.connection_under_lock(rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,|c| {
                    let mut query=c.prepare("SELECT source,origin_id,content_json FROM session_permission_authority WHERE conversation_id=?1 ORDER BY id")?;
                    let rows=query.query_map([owner.id()],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?)))?;
                    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
                })?;
                child.write_locked("refresh child authority",|w|{
                    let mut insert=w.0.prepare_cached("INSERT OR IGNORE INTO session_permission_authority(conversation_id,source,origin_id,content_json) VALUES(?1,?2,?3,?4)")?;
                    for (source,id,content) in &authority{insert.execute(params![child.id(),source,id,content])?;}
                    Ok(())
                })
            }
            _ => anyhow::bail!("Child belongs to another store"),
        }
    }
}

impl SessionRepository<'_> {
    pub(crate) fn has_worker_report(&self, report: &str) -> Result<bool> {
        self.read(|c, id| {
            Ok(
                c.prepare_cached("SELECT 1 FROM messages WHERE conversation_id=?1 AND id=?2")?
                    .exists([id, report])?,
            )
        })
    }
    pub(crate) fn record_worker_report(&self, report: &str, text: &str) -> Result<bool> {
        self.write("record finished worker report", |c, id| {
            if message_repo::get_message(c, report)?.is_some() {
                return Ok(false);
            }
            let order = c.query_row(
                "SELECT COALESCE(MAX(sort_order),-1)+1 FROM messages WHERE conversation_id=?1",
                [id],
                |r| r.get(0),
            )?;
            let now = chrono::Utc::now().to_rfc3339();
            message_repo::insert_message(c, report, id, "system", text, None, &now, false, order)?;
            if matches!(self.0, Backend::Shared(..)) {
                crate::db::conversation_repo::increment_message_count(c, id)?;
                crate::db::conversation_repo::update_conversation_timestamp(c, id, &now)?;
            }
            Ok(true)
        })
    }
}

impl ExecutorHistory {
    pub(crate) fn save_research_run(
        &self,
        run: &crate::services::deep_research::Run,
    ) -> Result<()> {
        ensure!(
            self.id() == run.parent_session_id,
            "Research run belongs to another session"
        );
        let encoded = serde_json::to_string(run)?;
        self.child_repo().write("save research workflow", |c, _| {
            super::state::set_local(c, &format!("workflow.{}", run.id), &encoded)
        })
    }
}
