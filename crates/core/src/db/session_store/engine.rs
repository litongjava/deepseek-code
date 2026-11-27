//! Storage used by the engine before a turn starts and after its event stream closes.
use super::{CreateOptions, ExecutorHistory, Stores, WRITE_DEADLINE};
use crate::db::{conversation_repo as catalog, message_repo as history, project_repo, DbManager};
use crate::native_engine::workspace;
use anyhow::{ensure, Result};
use rusqlite::{params, Connection};
use std::path::{Path, PathBuf};

type SavedHistory = (Option<String>, Vec<history::MessageRow>);
type Workspace = (
    PathBuf,
    Option<project_repo::ProjectRow>,
    crate::prompt::PromptProfile,
);

fn workspace_at(
    c: &Connection,
    root: &Path,
    id: &str,
    requested: Option<&str>,
) -> Result<Workspace> {
    let (cwd, project) = workspace::resolve_workspace(c, root, id, requested)?;
    Ok((cwd, project, workspace::conversation_prompt_profile(c, id)?))
}

impl Stores {
    pub(crate) fn engine_workspace(
        &self,
        root: &Path,
        id: &str,
        requested: Option<&str>,
    ) -> Result<Workspace> {
        let key = super::layout::storage_key(id)?;
        let _guard = self.resolved_key(&key)?;
        self.global().read(|c| {
            let tx = c.unchecked_transaction()?;
            if let Some(row) = catalog::conversation_metadata(&tx, id)? {
                ensure!(!row.archived, "Session is archived");
                if row.project_id.is_none() {
                    let path = super::lifecycle::workspace(&tx, root, id)?;
                    let profile =
                        crate::prompt::PromptProfile::from_workspace_mode(&row.workspace_mode);
                    return Ok((path, None, profile));
                }
            }
            workspace_at(&tx, root, id, requested)
        })
    }

    pub(crate) fn engine_history(&self, id: &str) -> Result<Option<SavedHistory>> {
        let Some(entry) = self
            .global()
            .read_directory_row(id, |c| super::directory::one(c, id))?
        else {
            return Ok(None);
        };
        let messages = self
            .open_listed(&entry)?
            .read(|r| crate::db::model_results::messages(r.0, id))?;
        Ok(Some((entry.conversation.model, messages)))
    }

    pub(crate) fn save_engine_state(
        &self,
        id: &str,
        model: &str,
        activity: &str,
        _turn_count: i64,
    ) -> Result<()> {
        let key = super::layout::storage_key(id)?;
        if let Some(_guard) = self.resolved_key(&key)? {
            let updated = self.global().write("save engine state", id, false, |tx| Ok(tx.execute(
                "UPDATE conversations SET model=?2, updated_at=CASE WHEN julianday(updated_at)<julianday(?3) THEN ?3 ELSE updated_at END,
                metadata_revision=metadata_revision+1 WHERE id=?1", params![id,model,activity])?))?;
            if updated == 1 {
                return Ok(());
            }
        }
        // The transcript projection owns message_count. An in-memory turn count
        // is not the number of durable messages.
        self.create(
            id,
            CreateOptions {
                model: Some(model.into()),
                ..Default::default()
            },
        )?;
        Ok(())
    }
}

impl DbManager {
    pub(crate) fn engine_workspace(
        &self,
        root: &Path,
        id: &str,
        requested: Option<&str>,
    ) -> Result<Workspace> {
        self.with_conn(|c| workspace_at(c, root, id, requested))?
    }
    pub(crate) fn engine_history(&self, id: &str) -> Result<Option<SavedHistory>> {
        self.with_conn(|c| {
            let tx = c.unchecked_transaction()?;
            let row = catalog::conversation_metadata(&tx, id)?;
            let messages = crate::db::model_results::messages(&tx, id)?;
            Ok(if row.is_none() && messages.is_empty() {
                None
            } else {
                Some((row.and_then(|r| r.model), messages))
            })
        })?
    }
    pub(crate) fn save_engine_state(
        &self,
        id: &str,
        model: &str,
        at: &str,
        count: i64,
    ) -> Result<()> {
        self.with_conn(|c| {
            crate::db::write_tx(c, "save engine state", WRITE_DEADLINE, |tx| {
                if catalog::conversation_metadata(tx, id)?.is_some() {
                    catalog::update_conversation(
                        tx,
                        id,
                        None,
                        Some(model),
                        None,
                        None,
                        None,
                        None,
                        None,
                        None,
                        Some(at),
                        Some(count),
                    )?;
                } else {
                    catalog::insert_conversation(
                        tx,
                        id,
                        None,
                        Some(model),
                        None,
                        None,
                        None,
                        false,
                        false,
                        false,
                        at,
                        at,
                        count,
                    )?;
                }
                Ok(())
            })
        })?
    }
}

impl ExecutorHistory {
    pub(crate) fn finish_response(&self, text: &str) -> Result<()> {
        match self {
            Self::Session(s) => s.write(|w| w.append("assistant",text).map(drop)),
            Self::Shared(db,id) => db.with_conn(|c|crate::db::write_tx(c,"append engine response",WRITE_DEADLINE,|tx|{
                let next=tx.query_row("SELECT COALESCE(MAX(sort_order),-1)+1 FROM messages WHERE conversation_id=?1",[id],|r|r.get(0))?;
                history::insert_message(tx,&uuid::Uuid::new_v4().to_string(),id,"assistant",text,None,&chrono::Utc::now().to_rfc3339(),false,next)?;
                catalog::increment_message_count(tx,id)?;
                Ok(())
            }))?,
        }
    }

    pub(crate) fn title_seed(&self) -> Result<Option<(String, Option<String>)>> {
        let eligible = |c: &Connection, id: &str| -> Result<bool> {
            Ok(catalog::conversation_metadata(c, id)?
                .is_some_and(|r| r.title.as_deref().is_none_or(|s| s.is_empty())))
        };
        let generate = match self {
            Self::Session(s) => {
                let _guard = s.stores.key_lock(&s.key, false, false)?;
                s.check_directory()?;
                s.stores.global().read(|c| eligible(c, s.id()))?
            }
            Self::Shared(db, id) => db.with_conn(|c| eligible(c, id))??,
        };
        if !generate {
            return Ok(None);
        }
        self.read(|c,id| {
            use rusqlite::OptionalExtension;
            let tx = if c.is_autocommit() {Some(c.unchecked_transaction()?)} else {None};
            let conn = tx.as_deref().unwrap_or(c);
            let first = |role: &str| -> Result<Option<String>> {Ok(conn.query_row(
                "SELECT content FROM messages WHERE conversation_id=?1 AND role=?2 ORDER BY sort_order,rowid LIMIT 1",
                [id,role], |r|r.get(0)).optional()?)};
            Ok(match first("user")? {Some(user)=>Some((user,first("assistant")?)),None=>None})
        })
    }

    pub fn publish_title(&self, title: &str) -> Result<bool> {
        match self {
            Self::Session(s) => {
                let _guard = s.stores.key_lock(&s.key, false, false)?;
                s.check_directory()?;
                s.stores.global().write("publish generated title",s.id(),false,|tx|{
                    let updated=catalog::update_title_if_empty(tx,s.id(),title)?;
                    if updated {ensure!(tx.execute("UPDATE conversations SET metadata_revision=metadata_revision+1 WHERE id=?1 AND incarnation=?2",[s.id(),s.incarnation()])?==1,"Session was replaced");}
                    Ok(updated)
                })
            }
            Self::Shared(db, id) => {
                db.with_conn(|c| catalog::update_title_if_empty(c, id, title))?
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_response_and_delayed_title_belong_to_the_admitted_incarnation() -> Result<()> {
        let root = tempfile::tempdir()?;
        let stores = Stores::open(root.path())?;
        assert!(stores.engine_history("chat")?.is_none());
        stores.save_engine_state("chat", "model", "2026-10-01T00:00:00Z", 99)?;
        let shard = stores.open_session("chat")?;
        let history = stores.executor_history("chat")?;
        assert_eq!(
            stores
                .engine_workspace(
                    &crate::data_layout::DataLayout::from_root(&root.path()).workspaces(),
                    "chat",
                    None
                )?
                .0,
            shard.workspace()?
        );
        shard.write(|w| w.append("user", "question").map(drop))?;
        history.finish_response("response")?;
        assert_eq!(
            history.title_seed()?,
            Some(("question".into(), Some("response".into())))
        );
        assert!(history.publish_title("generated")?);
        assert!(!history.publish_title("stale")?);
        let (model, rows) = stores.engine_history("chat")?.unwrap();
        assert_eq!(model.as_deref(), Some("model"));
        assert_eq!(rows.len(), 2);
        stores.save_engine_state("chat", "new", "2026-10-01T00:00:00Z", 200)?;
        assert_eq!(
            stores.global().conversation("chat")?.unwrap().message_count,
            2
        );
        shard.delete()?;
        let next = stores.create("chat", Default::default())?;
        assert!(history.finish_response("late").is_err());
        assert!(history.publish_title("late").is_err());
        assert!(next.read(|r| r.messages())?.is_empty());
        Ok(())
    }
}
