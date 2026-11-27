//! Directory-owned project and worktree bindings for one admitted conversation.
use super::{owned::Backend, shared::DirectoryRepository, SessionRepository};
use anyhow::{ensure, Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde_json::{json, Value};
use std::path::Path;

impl SessionRepository<'_> {
    pub fn workspace_project(&self) -> Result<Option<crate::db::project_repo::ProjectRow>> {
        self.directory_snapshot(crate::native_engine::workspace::conversation_project)
    }

    pub(super) fn directory_snapshot<T>(
        &self,
        body: impl FnOnce(&Connection, &str) -> Result<T>,
    ) -> Result<T> {
        match &self.0 {
            Backend::Shared(db, id) => db.directory_read(|c| {
                let tx = c.unchecked_transaction()?;
                body(&tx, id)
            }),
            Backend::Admitted(s) => {
                let _guard = s.stores.key_lock(&s.key, false, false)?;
                s.stores.global().read(|c| {
                let tx = c.unchecked_transaction()?;
                ensure!(tx.query_row("SELECT EXISTS(SELECT 1 FROM conversations WHERE id=?1 AND incarnation=?2)",[s.id(),s.incarnation()],|r|r.get::<_,bool>(0))?, "Session was replaced");
                body(&tx,s.id())
            })
            }
        }
    }

    pub(super) fn update_workspace<T>(
        &self,
        label: &str,
        mut body: impl FnMut(&Transaction<'_>, &str) -> Result<T>,
    ) -> Result<T> {
        let deadline = crate::db::STARTUP_WRITE_DEADLINE;
        match &self.0 {
            Backend::Shared(db, id) => db.directory_write(label, id, deadline, |tx| body(tx, id)),
            Backend::Admitted(s) => {
                let _guard = s.stores.key_lock(&s.key, false, false)?;
                s.stores.global().write_with_deadline(label,s.id(),false,deadline,|tx| {
                    ensure!(tx.execute("UPDATE conversations SET metadata_revision=metadata_revision+1 WHERE id=?1 AND incarnation=?2",[s.id(),s.incarnation()])?==1,"Session was replaced");
                    let value = body(tx, s.id())?;
                    let mut guards = Default::default();
                    s.stores.admit_workspace_change(tx, s.id(), s.incarnation(), &mut guards)?;
                    Ok((value, guards))
                }).map(|(value, _admission)| value)
            }
        }
    }

    pub fn worktree_status(&self) -> Result<Option<Value>> {
        self.directory_snapshot(|conn,id| Ok(conn.query_row(
            "SELECT w.source_path,w.path,w.branch,COALESCE(b.backend,'git') FROM conversation_worktrees w LEFT JOIN conversation_worktree_backends b USING(conversation_id) WHERE w.conversation_id=?1",[id],
            |r|Ok(json!({"source":r.get::<_,String>(0)?,"path":r.get::<_,String>(1)?,"name":r.get::<_,String>(2)?,"backend":r.get::<_,String>(3)?}))).optional()?))
    }

    pub fn bind_prepared_worktree(
        &self,
        source: &Path,
        workspace: &crate::worktree::session::PreparedWorkspace,
    ) -> Result<()> {
        self.update_workspace("session worktree binding", |tx, id| {
            let project = validated_source(tx, id, source)?;
            workspace.save(tx, id, &project, &source.to_string_lossy())?;
            set_path(tx, id, &workspace.path)
        })
    }

    pub(crate) fn bind_worktree(
        &self,
        source: &Path,
        repository: &Path,
        path: &Path,
        branch: &str,
        hook_metadata: Option<&Value>,
    ) -> Result<()> {
        self.update_workspace("worktree binding",|tx,id| {
            let project=validated_source(tx,id,source)?;
            tx.execute("INSERT INTO conversation_worktrees(conversation_id,project_id,source_path,repo_path,path,branch) VALUES(?1,?2,?3,?4,?5,?6)",
                params![id,project,source.to_string_lossy(),repository.to_string_lossy(),path.to_string_lossy(),branch])?;
            if let Some(metadata)=hook_metadata {
                tx.execute("INSERT INTO conversation_worktree_backends(conversation_id,backend,metadata) VALUES(?1,'hook',?2)",params![id,metadata.to_string()])?;
            }
            set_path(tx,id,path)
        })
    }

    pub(crate) fn check_worktree_source(&self, source: &Path) -> Result<()> {
        self.directory_snapshot(|c, id| validated_source(c, id, source).map(drop))
    }

    pub(crate) fn removal_metadata(&self, source: &Path, path: &Path) -> Result<Option<Value>> {
        let raw=self.directory_snapshot(|c,id| {
            let project=validated_source(c,id,source)?;
            let bound=crate::worktree::session::resolve(c,id,&project,source)?.context("Worktree binding missing")?;
            ensure!(bound==path,"Session worktree binding changed");
            let shared=c.query_row("SELECT COUNT(*) FROM conversation_worktrees WHERE path=?1",[path.to_string_lossy().as_ref()],|r|r.get::<_,i64>(0))?;
            ensure!(shared==1,"Other sessions share this worktree; detach those bindings before removal");
            Ok(c.query_row("SELECT metadata FROM conversation_worktree_backends WHERE conversation_id=?1 AND backend='hook'",[id],|r|r.get::<_,String>(0)).optional()?)
        })?;
        raw.map(|s| serde_json::from_str(&s))
            .transpose()
            .map_err(Into::into)
    }

    pub(crate) fn detach_worktree(&self, source: &Path, path: &Path) -> Result<()> {
        self.update_workspace("detach worktree",|tx,id| {
            tx.execute("DELETE FROM conversation_worktree_backends WHERE conversation_id=?1",[id])?;
            ensure!(tx.execute("DELETE FROM conversation_worktrees WHERE conversation_id=?1 AND source_path=?2 AND path=?3",params![id,source.to_string_lossy(),path.to_string_lossy()])?==1,"Session worktree binding changed");
            set_path(tx,id,source)
        })
    }
}

fn set_path(c: &Connection, id: &str, path: &Path) -> Result<()> {
    ensure!(
        c.execute(
            "UPDATE conversations SET workspace_path=?1 WHERE id=?2",
            params![path.to_string_lossy(), id]
        )? == 1,
        "Session not found"
    );
    Ok(())
}

fn validated_source(c: &Connection, id: &str, source: &Path) -> Result<String> {
    let project = crate::native_engine::workspace::conversation_project(c, id)?
        .context("Worktree requires a trusted project")?;
    let stored = project
        .workspace_path
        .as_deref()
        .context("Project source missing")?;
    ensure!(
        Path::new(stored).canonicalize()? == source.canonicalize()?,
        "Worktree source does not match the trusted project"
    );
    Ok(project.id)
}

impl SessionRepository<'_> {
    pub(crate) fn change_workspace(
        &self,
        base: &Path,
        target: &Path,
        add: bool,
    ) -> Result<std::path::PathBuf> {
        self.update_workspace("session workspace change", |tx, id| {
            crate::services::session_workspace::change_in(tx, id, base, target, add)
        })
    }
    pub(crate) fn inspect_project_trust(&self) -> Result<crate::db::project_repo::ProjectRow> {
        self.directory_snapshot(crate::services::session_trust::project)
    }
    pub(crate) fn repair_project_trust(
        &self,
        root: &Path,
        folders: &[String],
    ) -> Result<crate::services::session_trust::Repaired> {
        self.update_workspace("repair session trust", |tx, id| {
            crate::services::session_trust::repair_in(tx, root, id, folders)
        })
    }
}

impl SessionRepository<'_> {
    pub(crate) fn move_child_workspace(
        &self,
        root: &Path,
        id: &str,
        workspace: &Path,
    ) -> Result<()> {
        use crate::native_engine::tool_loop::{RealShellCwdFs, ShellCwdReset};
        let reset = ShellCwdReset::prepare_with(&RealShellCwdFs, root, id)?;
        match self.update_workspace("rebind session workspace", |tx, owner| {
            ensure!(owner == id, "Session identity changed");
            crate::native_engine::workspace::rebind_in(tx, owner, workspace)
        }) {
            Ok(()) => reset.commit_with(&RealShellCwdFs),
            Err(error) => match reset.restore_with(&RealShellCwdFs) {
                Ok(()) => Err(error),
                Err(restore) => Err(error.context(format!(
                    "the shell state could not be restored either: {restore:#}"
                ))),
            },
        }
    }
    pub(crate) fn initialize_child_model(&self, model: &str, provider: &str) -> Result<()> {
        self.update_workspace("initialize child model", |tx, id| {
            ensure!(
                tx.execute(
                    "UPDATE conversations SET model=?2,provider=?3 WHERE id=?1",
                    params![id, model, provider]
                )? == 1,
                "Child session disappeared"
            );
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::session_store::{SessionAccess, Stores};

    #[test]
    fn workspace_edits_and_delayed_worktree_binding_follow_the_selected_incarnation() -> Result<()>
    {
        let root = tempfile::tempdir()?;
        let source = root.path().join("source");
        let extra = root.path().join("extra");
        std::fs::create_dir(&source)?;
        std::fs::create_dir(&extra)?;
        let stores = Stores::open(root.path())?;
        let id = uuid::Uuid::new_v4().to_string();
        let shard = stores.create_code_session(&source, "model", "provider", &id)?;
        let owner = shard.select_session(&id)?;
        let changed = owner.change_workspace(&source, &extra, true)?;
        assert_eq!(changed.canonicalize()?, source.canonicalize()?);
        let project = owner.inspect_project_trust()?;
        assert_eq!(project.folders.len(), 2);
        let repaired = owner.repair_project_trust(
            root.path(),
            &project
                .folders
                .into_iter()
                .map(|f| f.path)
                .collect::<Vec<_>>(),
        )?;
        assert_ne!(repaired.project_id, project.id);
        let prepared = crate::worktree::session::prepare(&source, None, false)?;
        shard.delete()?;
        stores.create_code_session(&extra, "model", "provider", &id)?;
        assert!(owner.bind_prepared_worktree(&source, &prepared).is_err());
        assert!(owner.worktree_status().is_err());
        assert_eq!(
            std::path::Path::new(
                stores
                    .select_session(&id)?
                    .inspect_project_trust()?
                    .workspace_path
                    .as_deref()
                    .unwrap()
            )
            .canonicalize()?,
            extra.canonicalize()?
        );
        Ok(())
    }
}

impl super::Stores {
    pub fn workspace_roots(
        &self,
        scope: &crate::services::workspace_files::Scope,
    ) -> Result<Vec<crate::services::workspace_files::Root>> {
        let root = self.layout().workspaces();
        if let Some(id) = scope.conversation_id.as_deref() {
            use super::SessionAccess;
            return self.select_session(id)?.directory_snapshot(|c,_| crate::services::workspace_files::roots(c,&root,scope));
        }
        self.global().read(|c| {
            let snapshot=c.unchecked_transaction()?;
            crate::services::workspace_files::roots(&snapshot,&root,scope)
        })
    }
}
