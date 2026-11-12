//! Session-local workspace changes using the same project trust model as Desktop.
use crate::{
    db::{project_repo, DbManager},
    native_engine::workspace,
};
use anyhow::{ensure, Context, Result};
use std::path::{Path, PathBuf};

pub fn change(db: &DbManager, id: &str, base: &Path, argument: &str, add: bool) -> Result<PathBuf> {
    let raw = argument.trim().trim_matches('"');
    ensure!(!raw.is_empty(), "A directory is required");
    let path = Path::new(raw);
    let target = std::fs::canonicalize(if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    })
    .context("Directory is unavailable")?;
    ensure!(target.is_dir(), "Path must be a directory");
    db.with_conn(|conn| -> Result<PathBuf> {
        let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
        let original = workspace::conversation_project(&tx, id)?.context("Session has no project")?;
        let has_worktree: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM conversation_worktrees WHERE conversation_id=?1)", [id], |r| r.get(0))?;
        ensure!(!has_worktree || add, "This session uses a managed worktree; start a separate session to replace its working folder");
        let managed_workspace=if has_worktree {Some(workspace::resolve_workspace(&tx,base,id,None)?.0)} else {None};
        let mut folders = if add { original.folders.iter().map(|f| f.path.clone()).collect::<Vec<_>>() } else { Vec::new() };
        let target_string = target.to_string_lossy().into_owned();
        if !folders.contains(&target_string) { folders.push(target_string); }
        let folders = project_repo::normalize_folders(&folders)?;
        let project_id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now().to_rfc3339();
        project_repo::insert_project(&tx, &project_id, &original.name, original.description.as_deref(), original.instructions.as_deref(), Some(&folders[0]), false, &now, &now)?;
        project_repo::set_folders(&tx, &project_id, &folders)?;
        // The explicit command selects these folders, like CLI --path.
        project_repo::trust_folders(&tx, &project_id, &folders)?;
        ensure!(tx.execute("UPDATE conversations SET project_id=?1, workspace_path=?2, updated_at=?3 WHERE id=?4", rusqlite::params![project_id, folders[0], now, id])? == 1, "Session not found");
        if let Some(path)=&managed_workspace {
            tx.execute("UPDATE conversation_worktrees SET project_id=?1 WHERE conversation_id=?2",rusqlite::params![project_id,id])?;
            tx.execute("UPDATE conversations SET workspace_path=?1 WHERE id=?2",rusqlite::params![path.to_string_lossy(),id])?;
        }
        tx.commit()?;
        Ok(managed_workspace.unwrap_or_else(||PathBuf::from(&folders[0])))
    })?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::SessionStore;
    use std::sync::Arc;

    #[test]
    fn changes_are_isolated_persistent_and_validate_directories() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first");
        let second = root.path().join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let store = SessionStore::new(Arc::new(
            DbManager::new(root.path().join("db.sqlite")).unwrap(),
        ));
        store.db.init().unwrap();
        let a = store.create(&first, "model", "provider").unwrap();
        let b = store.create(&first, "model", "provider").unwrap();
        change(&store.db, &a, &first, second.to_str().unwrap(), true).unwrap();
        store
            .db
            .with_conn(|conn| {
                assert_eq!(
                    workspace::conversation_project(conn, &a)
                        .unwrap()
                        .unwrap()
                        .folders
                        .len(),
                    2
                );
                assert_eq!(
                    workspace::conversation_project(conn, &b)
                        .unwrap()
                        .unwrap()
                        .folders
                        .len(),
                    1
                );
            })
            .unwrap();
        assert!(change(&store.db, &a, &first, "absent", false).is_err());
        let result = change(&store.db, &a, &first, second.to_str().unwrap(), false).unwrap();
        assert_eq!(
            std::fs::canonicalize(&result).unwrap(),
            std::fs::canonicalize(second).unwrap()
        );
        assert_eq!(store.workspace(&a, root.path()).unwrap(), result);
        assert_eq!(
            std::fs::canonicalize(store.workspace(&b, root.path()).unwrap()).unwrap(),
            std::fs::canonicalize(first).unwrap()
        );
    }

    #[test]
    fn new_session_does_not_inherit_added_folders() {
        let root = tempfile::tempdir().unwrap();
        let first = root.path().join("first");
        let second = root.path().join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let store = SessionStore::new(Arc::new(
            DbManager::new(root.path().join("db.sqlite")).unwrap(),
        ));
        store.db.init().unwrap();
        let a = store.create(&first, "model", "provider").unwrap();
        change(&store.db, &a, &first, second.to_str().unwrap(), true).unwrap();
        std::fs::remove_dir(&second).unwrap();
        let c = store.create(&first, "model", "provider").unwrap();
        store
            .db
            .with_conn(|conn| {
                assert_eq!(
                    workspace::conversation_project(conn, &c)
                        .unwrap()
                        .unwrap()
                        .folders
                        .len(),
                    1
                );
            })
            .unwrap();
        let third = root.path().join("third");
        std::fs::create_dir(&third).unwrap();
        change(&store.db, &c, &first, third.to_str().unwrap(), true).unwrap();
    }
}
