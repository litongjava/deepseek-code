use super::*;
use crate::db::session_store::{Branch, SessionDb, Stores};

impl SessionStore<Stores> {
    pub fn create(&self, workspace: &Path, model: &str, provider: &str) -> Result<String> {
        self.create_with_id(
            workspace,
            model,
            provider,
            &uuid::Uuid::new_v4().to_string(),
        )
    }
    pub fn create_with_id(
        &self,
        workspace: &Path,
        model: &str,
        provider: &str,
        id: &str,
    ) -> Result<String> {
        Ok(self
            .db
            .create_code_session(workspace, model, provider, id)?
            .id()
            .to_owned())
    }
    pub fn resume(&self, id: &str) -> Result<conversation_repo::ConversationRow> {
        Ok(self.db.resume_code(id)?.1)
    }
    pub fn history(&self, id: &str) -> Result<Vec<Value>> {
        self.db.open_session(id)?.read(|r| {
            Ok(r.messages()?
                .into_iter()
                .map(|m| json!({"role":m.role,"content":m.content}))
                .collect())
        })
    }
    pub fn latest(&self, workspace: &Path) -> Result<Option<conversation_repo::ConversationRow>> {
        self.db.latest_code(workspace)
    }
    pub fn fork(&self, id: &str) -> Result<String> {
        Ok(self.fork_session(id)?.id().to_owned())
    }
    pub fn fork_session(&self, id: &str) -> Result<SessionDb> {
        self.branch(id, Branch::Full)
    }
    pub fn clear(&self, id: &str) -> Result<String> {
        Ok(self.branch(id, Branch::Clear)?.id().to_owned())
    }
    fn branch(&self, id: &str, boundary: Branch<'_>) -> Result<SessionDb> {
        let (parent, _) = self.db.resume_code(id)?;
        let id = uuid::Uuid::new_v4().to_string();
        parent.branch(&id, boundary)
    }
    pub fn derive_owned_child(&self, id: &str, root: &Path, expected_cwd: &Path) -> Result<String> {
        let parent = self.session_in_root(id, root)?;
        let child = uuid::Uuid::new_v4().to_string();
        parent.derive_owned_child(&child, expected_cwd)?;
        Ok(child)
    }
    fn session_in_root(&self, id: &str, root: &Path) -> Result<SessionDb> {
        // The store has canonicalized its data directory, but a caller may use
        // a Windows short path. The workspaces subdirectory need not exist yet.
        let root = std::path::absolute(root)?;
        let parent = root
            .parent()
            .context("Workspace root has no parent directory")?;
        let expected = self.db.layout().data_root().canonicalize()?;
        let actual = parent.canonicalize()?;
        ensure!(
            root.ends_with("workspaces") && actual == expected,
            "Workspace root differs from the session data root: root={} parent={} expected={}",
            root.display(),
            actual.display(),
            expected.display()
        );
        self.db.open_session(id)
    }
    pub fn workspace(&self, id: &str, root: &Path) -> Result<PathBuf> {
        self.session_in_root(id, root)?.workspace()
    }
    pub fn model_history_with_boundary(
        &self,
        id: &str,
        format: &ApiFormat,
    ) -> Result<(Vec<Value>, Option<String>)> {
        self.db.open_session(id)?.read(|r| r.model_history(format))
    }
    pub fn model_history(&self, id: &str, format: &ApiFormat) -> Result<Vec<Value>> {
        Ok(self.model_history_with_boundary(id, format)?.0)
    }
    pub fn append(&self, id: &str, role: &str, text: &str) -> Result<String> {
        self.db.open_session(id)?.write(|w| w.append(role, text))
    }
    pub fn append_prompt(&self, id: &str, text: &str, origin: &str) -> Result<String> {
        self.db.open_session(id)?.admit_prompt(text, None, origin)
    }
    pub fn append_user_content(
        &self,
        id: &str,
        text: &str,
        content: &Value,
        origin: &str,
    ) -> Result<String> {
        self.db
            .open_session(id)?
            .admit_prompt(text, Some(content), origin)
    }
    pub fn append_hook_user(&self, id: &str, text: &str) -> Result<String> {
        self.db
            .open_session(id)?
            .write(|w| w.append_hook_user(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_root_uses_the_existing_data_directory_identity() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let data = temp.path().join("data");
        let project = temp.path().join("project");
        std::fs::create_dir(&project)?;
        let store = SessionStore::new(RuntimePaths::new(data.clone())?.open_database()?);
        let id = store.create(&project, "model", "provider")?;
        let root = data.join("data").join("workspaces");
        assert!(!root.exists());
        assert_eq!(
            store.workspace(&id, &root)?.canonicalize()?,
            project.canonicalize()?
        );
        assert!(store.workspace(&id, &project.join("workspaces")).is_err());
        assert!(store.workspace(&id, &data.join("other")).is_err());
        Ok(())
    }
}
