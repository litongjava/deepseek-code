//! Resolve the same project and history for interactive hosts, SDKs and workers.
use super::{CreateOptions, ProjectSelection, SessionDb, SessionWrite, Stores};
use crate::db::conversation_repo::ConversationRow;
use anyhow::{ensure, Context, Result};
use rusqlite::params;
use serde_json::{json, Value};
use std::path::Path;

impl Stores {
    pub fn create_code_session(
        &self,
        workspace: &Path,
        model: &str,
        provider: &str,
        id: &str,
    ) -> Result<SessionDb> {
        let id = uuid::Uuid::parse_str(id)
            .context("Session ID must be a UUID")?
            .to_string();
        let path = workspace
            .canonicalize()
            .context("Project directory is unavailable")?;
        ensure!(path.is_dir(), "Project path must be a directory");
        let _admission = self.admit_workspace_path(&path)?;
        self.create(
            &id,
            CreateOptions {
                project: Some(ProjectSelection::Workspace(path)),
                workspace_mode: Some("code".into()),
                model: Some(model.into()),
                provider: Some(provider.into()),
                title: Some("CLI session".into()),
                ..Default::default()
            },
        )
    }

    pub fn resume_code(&self, id: &str) -> Result<(SessionDb, ConversationRow)> {
        let shard = self.open_session(id)?;
        let (metadata, _) = shard.code_selection()?;
        Ok((shard, metadata))
    }

    pub fn latest_code(&self, workspace: &Path) -> Result<Option<ConversationRow>> {
        let workspace = workspace
            .canonicalize()
            .context("Project directory is unavailable")?;
        ensure!(workspace.is_dir(), "Project path must be a directory");
        let rows = self.global().continuation_candidates()?;
        for row in rows {
            // Worktrees and private children can have a different directory from
            // the project's main path. Only an eligible selection opens history.
            let current = self.global().read_directory_row(&row.conversation.id, |c| {
                let snapshot = c.unchecked_transaction()?;
                let matches: bool = snapshot.query_row("SELECT EXISTS(SELECT 1 FROM conversations WHERE id=?1 AND incarnation=?2 AND archived=0 AND workspace_mode='code')",
                    [&row.conversation.id,&row.incarnation], |r|r.get(0))?;
                if !matches {return Ok(None);}
                let actual = super::lifecycle::workspace(&snapshot,&self.layout().workspaces(),&row.conversation.id);
                Ok(actual.ok().and_then(|p|p.canonicalize().ok()).filter(|p|p==&workspace))
            })?;
            if current.is_some() && self.open_listed(&row).is_ok() {
                return Ok(Some(row.conversation));
            }
        }
        Ok(None)
    }
}

impl SessionDb {
    pub fn code_selection(&self) -> Result<(ConversationRow, std::path::PathBuf)> {
        let _guard = self.stores.key_lock(&self.key, false, false)?;
        self.check_directory()?;
        self.stores.global().read(|c| {
            let tx = c.unchecked_transaction()?;
            let mut row = super::directory::one(&tx, self.id())?
                .context("Session not found")?
                .conversation;
            ensure!(
                row.workspace_mode == "code",
                "Only Code sessions can be resumed in CLI"
            );
            ensure!(!row.archived, "Session is archived");
            crate::native_engine::workspace::conversation_project(&tx, self.id())?;
            let path =
                super::lifecycle::workspace(&tx, &self.stores.layout().workspaces(), self.id())?;
            if row.project_id.is_none() {
                row.workspace_path = Some(path.to_string_lossy().into_owned());
            }
            Ok((row, path))
        })
    }
}

impl SessionWrite<'_> {
    pub(super) fn append_user_content(
        &self,
        display: &str,
        content: &Value,
        typed: bool,
    ) -> Result<String> {
        let content = crate::runtime::normalize_user_content(content)?;
        let message = self.append("user", display)?;
        self.0.execute(
            "INSERT INTO message_user_content(message_id,content_json) VALUES(?1,?2)",
            params![message, serde_json::to_string(&content)?],
        )?;
        if typed {
            crate::services::permission_authority::record_conn(
                self.0,
                self.1,
                crate::services::permission_authority::Source::User,
                &content,
            )?;
        }
        Ok(message)
    }

    pub fn append_hook_user(&self, text: &str) -> Result<String> {
        let message = self.append("user", text)?;
        self.set_message_metadata(
            &message,
            &json!({"type":"hook","hookEvent":"SessionStart","promptOrigin":"hook"}),
        )?;
        Ok(message)
    }
}
