use anyhow::{ensure, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};

use crate::db::{conversation_repo, project_repo};

/// Rebinds one conversation to a host-authorized working directory.
///
/// `resolve_workspace` derives the prompt and tool directory from the session's
/// project, not from `conversations.workspace_path`. Updating only the row would
/// therefore leave a resumed member executing in its previous checkout. This
/// creates (or reuses) a trusted project for the requested directory and points
/// only this conversation at it, so a shared project is never redirected for
/// other sessions. The conversation ID, transcript, role and ownership are
/// unchanged.
///
/// Callers must have an explicit host authorization for the move; the ordinary
/// child-resume path refuses a workspace change unless the launcher set
/// `allow_workspace_move`.
pub fn rebind_conversation_workspace(conn: &Connection, id: &str, workspace: &Path) -> Result<()> {
    let workspace = workspace
        .canonicalize()
        .context("Workspace is unavailable")?;
    ensure!(workspace.is_dir(), "Workspace must be a directory");
    let path = workspace.to_string_lossy().into_owned();
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    let conversation =
        conversation_repo::get_conversation(&tx, id)?.context("Session not found")?;
    // A managed worktree owns its own resolution path; rebinding the project
    // underneath it would make the binding inconsistent.
    let managed: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM conversation_worktrees WHERE conversation_id=?1)",
        [id],
        |row| row.get(0),
    )?;
    ensure!(
        !managed,
        "A session with a managed worktree cannot move to another workspace"
    );
    let original = conversation
        .project_id
        .as_deref()
        .and_then(|project_id| project_repo::get_project(&tx, project_id).ok().flatten());
    let name = original
        .as_ref()
        .map(|project| project.name.clone())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| {
            workspace
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        });
    let description = original
        .as_ref()
        .and_then(|project| project.description.as_deref());
    let instructions = original
        .as_ref()
        .and_then(|project| project.instructions.as_deref());
    let project_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    project_repo::insert_project(
        &tx,
        &project_id,
        &name,
        description,
        instructions,
        Some(&path),
        false,
        &now,
        &now,
    )?;
    project_repo::set_folders(&tx, &project_id, std::slice::from_ref(&path))?;
    // The host selected this directory for the member; the same explicit
    // trust decision is recorded when a new child session is created there.
    project_repo::trust_folders(&tx, &project_id, std::slice::from_ref(&path))?;
    ensure!(
        tx.execute(
            "UPDATE conversations SET project_id=?1, workspace_path=?2, updated_at=?3 WHERE id=?4",
            params![project_id, path, now, id],
        )? == 1,
        "Session not found"
    );
    tx.commit()?;
    Ok(())
}

/// Moves one session to a host-authorized workspace and keeps its shell cache
/// consistent with the result.
///
/// The session's saved foreground-shell cwd is staged first. If the database
/// rebind fails it is restored exactly; if the rebind commits, the staged copy
/// is dropped. A staging or cleanup failure is reported rather than silently
/// ignored, and a cleanup failure after a committed move never restores the
/// cache. Ordinary resumes and `cd` persistence never reach this function.
pub fn move_conversation_workspace(
    conn: &Connection,
    data_dir: &Path,
    id: &str,
    workspace: &Path,
) -> Result<()> {
    move_conversation_workspace_with(
        conn,
        data_dir,
        id,
        workspace,
        &super::tool_loop::RealShellCwdFs,
    )
}

pub(crate) fn move_conversation_workspace_with(
    conn: &Connection,
    data_dir: &Path,
    id: &str,
    workspace: &Path,
    fs: &dyn super::tool_loop::ShellCwdFs,
) -> Result<()> {
    let reset = super::tool_loop::ShellCwdReset::prepare_with(fs, data_dir, id)?;
    match rebind_conversation_workspace(conn, id, workspace) {
        Ok(()) => reset.commit_with(fs),
        Err(error) => match reset.restore_with(fs) {
            Ok(()) => Err(error),
            Err(restore) => Err(error.context(format!(
                "the shell state could not be restored either: {restore:#}"
            ))),
        },
    }
}

/// Use persisted mode, never infer Code from a directory, model, or client prompt.
pub fn conversation_prompt_profile(conn: &Connection, id: &str) -> Result<crate::prompt::PromptProfile> {
    Ok(conversation_repo::get_conversation(conn, id)?
        .map(|conversation| crate::prompt::PromptProfile::from_workspace_mode(&conversation.workspace_mode))
        .unwrap_or_default())
}

pub fn conversation_project(conn: &Connection, id: &str) -> Result<Option<project_repo::ProjectRow>> {
    let conversation = conversation_repo::get_conversation(conn, id)?;
    if let Some(conversation) = conversation {
        if let Some(project_id) = conversation.project_id.filter(|id| !id.is_empty()) {
            let project = project_repo::require_trusted(conn, &project_id)?;
            ensure!(!project.is_archived, "Select an active project before sending messages");
            return Ok(Some(project));
        }
    }
    Ok(None)
}

/// Explicit owned-child bindings are the only exception to one private directory per session.
fn private_workspace_owner(conn: &Connection, id: &str) -> Result<Option<String>> {
    let binding = |id: &str| -> Result<Option<String>> {
        Ok(conn.query_row("SELECT parent_id FROM conversation_private_workspace_bindings WHERE child_id=?1", [id], |row| row.get(0)).optional()?)
    };
    if binding(id)?.is_none() { return Ok(None); }
    let child = conversation_repo::get_conversation(conn,id)?.context("Private child not found")?;
    let mode = child.workspace_mode;
    let mut current=id.to_owned();
    let mut seen=std::collections::HashSet::new();
    for _ in 0..16 {
        ensure!(seen.insert(current.clone()), "Private workspace ownership cycle");
        let session=conversation_repo::get_conversation(conn,&current)?.context("Private workspace owner not found")?;
        ensure!(!session.archived && session.workspace_mode==mode && session.project_id.as_deref().is_none_or(str::is_empty), "Private workspace ownership mode/project/trust changed");
        match binding(&current)? {Some(parent)=>current=parent,None=>return Ok(Some(current))}
    }
    anyhow::bail!("Private workspace ownership depth exceeded")
}

/// A Home conversation always owns its working directory. Legacy paths and client
/// overrides must not turn an unassigned conversation into access to a source tree.
pub fn resolve_workspace(
    conn: &Connection,
    workspaces_root: &Path,
    id: &str,
    requested: Option<&str>,
) -> Result<(PathBuf, Option<project_repo::ProjectRow>)> {
    let private_owner = private_workspace_owner(conn, id)?;
    if let Some(mut project) = conversation_project(conn, id)? {
        let path = project.workspace_path.as_deref().context("Project has no working directory")?;
        ensure!(Path::new(path).is_absolute() && Path::new(path).is_dir(), "Project folder is unavailable");
        if let Some(worktree) = crate::worktree::session::resolve(conn, id, &project.id, Path::new(path))? {
            // The prompt and tools must agree on the actual working folder.
            project.workspace_path = Some(worktree.to_string_lossy().into_owned());
            if let Some(folder) = project.folders.first_mut() {
                folder.path = worktree.to_string_lossy().into_owned();
            }
            return Ok((worktree, Some(project)));
        }
        return Ok((PathBuf::from(path), Some(project)));
    }

    let conversation = conversation_repo::get_conversation(conn, id)?;
    // Internal jobs without a desktop conversation may explicitly supply their cwd.
    if conversation.is_none() {
        if let Some(path) = requested.filter(|path| !path.trim().is_empty()) {
            let path = PathBuf::from(path);
            ensure!(path.is_absolute() && path.is_dir(), "Working directory must be an existing absolute path");
            return Ok((path, None));
        }
    }

    ensure!(!id.is_empty() && id.len() <= 128 && id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'), "Invalid conversation workspace ID");
    ensure!(workspaces_root.is_absolute(), "Application workspace directory must be absolute");
    std::fs::create_dir_all(workspaces_root).context("Could not create application workspace directory")?;
    let root = std::fs::canonicalize(workspaces_root)?;
    let owner_id = private_owner.as_deref().unwrap_or(id);
    ensure!(!owner_id.is_empty() && owner_id.len()<=128 && owner_id.bytes().all(|c|c.is_ascii_alphanumeric() || c==b'-' || c==b'_'), "Invalid private owner workspace ID");
    let expected = root.join(owner_id);
    let directory = expected.clone();
    std::fs::create_dir_all(&directory).context("Could not create conversation workspace")?;
    let directory = std::fs::canonicalize(directory)?;
    ensure!(directory.starts_with(&root) && directory != root, "Conversation workspace is outside the application workspace directory");
    if private_owner.is_some() { ensure!(directory == expected, "Private workspace owner path is redirected"); }
    if conversation.is_some() {
        conn.execute("UPDATE conversations SET workspace_path = ?1 WHERE id = ?2", params![directory.to_string_lossy(), id])?;
    }
    Ok((directory, None))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(mode: &str) -> (Connection, tempfile::TempDir) {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::db::schema::SCHEMA_SQL).unwrap();
        conversation_repo::migrate_workspace_mode(&conn).unwrap();
        conversation_repo::insert_conversation(&conn, "session", None, None, None, Some("."), None, false, false, false, "now", "now", 0).unwrap();
        conversation_repo::set_workspace_mode(&conn, "session", mode).unwrap();
        (conn, tempfile::tempdir().unwrap())
    }

    #[test]
    fn prompt_profile_follows_saved_mode_and_defaults_for_internal_jobs() {
        for mode in ["chat", "work", "study", "code"] {
            let (conn, _root) = fixture(mode);
            let profile = conversation_prompt_profile(&conn, "session").unwrap();
            assert_eq!(profile == crate::prompt::PromptProfile::Code, mode == "code");
            assert_eq!(conversation_prompt_profile(&conn, "internal-job").unwrap(), crate::prompt::PromptProfile::Desktop);
        }
    }

    #[test]
    fn home_modes_get_persistent_private_directories_instead_of_legacy_or_requested_paths() {
        for mode in ["chat", "work", "study"] {
            let (conn, root) = fixture(mode);
            let cwd = std::env::current_dir().unwrap();
            let (path, project) = resolve_workspace(&conn, root.path(), "session", cwd.to_str()).unwrap();
            assert!(project.is_none());
            assert_eq!(path, std::fs::canonicalize(root.path()).unwrap().join("session"));
            assert_ne!(path, cwd);
            std::fs::write(path.join("notes.txt"), "keep me").unwrap();
            assert_eq!(resolve_workspace(&conn, root.path(), "session", Some(".")).unwrap().0, path);
            assert_eq!(std::fs::read_to_string(path.join("notes.txt")).unwrap(), "keep me");
            assert_eq!(conversation_repo::get_conversation(&conn, "session").unwrap().unwrap().workspace_path.as_deref(), path.to_str());
            assert_ne!(resolve_workspace(&conn, root.path(), "another", None).unwrap().0, path);
        }
    }

    #[test]
    fn code_without_project_uses_private_workspace() {
        let (conn, root) = fixture("code");
        assert!(conversation_project(&conn, "session").unwrap().is_none());
        let (path, project) = resolve_workspace(&conn, root.path(), "session", root.path().to_str()).unwrap();
        assert!(project.is_none());
        assert_eq!(path, root.path().canonicalize().unwrap().join("session"));
    }

    #[test]
    fn selected_project_must_exist_and_be_trusted_and_overrides_requested_cwd() {
        let (conn, root) = fixture("code");
        project_repo::insert_project(&conn, "project", "Project", None, None, None, false, "now", "now").unwrap();
        let folder = root.path().join("source");
        std::fs::create_dir(&folder).unwrap();
        let folders = vec![folder.to_string_lossy().into_owned()];
        project_repo::set_folders(&conn, "project", &folders).unwrap();
        conn.execute("UPDATE conversations SET project_id = 'project' WHERE id = 'session'", []).unwrap();
        assert!(resolve_workspace(&conn, root.path(), "session", None).is_err());
        project_repo::trust_folders(&conn, "project", &folders).unwrap();
        let (path, project) = resolve_workspace(&conn, root.path(), "session", Some(".")).unwrap();
        assert_eq!(std::fs::canonicalize(path).unwrap(), std::fs::canonicalize(folder).unwrap());
        assert_eq!(project.unwrap().id, "project");
        conn.execute("UPDATE projects SET is_archived = 1 WHERE id = 'project'", []).unwrap();
        assert!(resolve_workspace(&conn, root.path(), "session", None).is_err());
    }

    #[test]
    fn workspace_paths_cannot_escape_via_conversation_ids_or_relative_roots() {
        let (conn, root) = fixture("chat");
        for id in ["", ".", "..", "../source", "..\\source", "C:\\source", "a/b", "a:b"] {
            assert!(resolve_workspace(&conn, root.path(), id, None).is_err(), "{id}");
        }
        assert!(resolve_workspace(&conn, Path::new("."), "session", None).is_err());
    }

    struct RebindFixture {
        _root: tempfile::TempDir,
        first: PathBuf,
        second: PathBuf,
        workspaces: PathBuf,
        store: crate::runtime::SessionStore,
        parent: String,
        member: String,
        sibling: String,
    }

    impl RebindFixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let first = root.path().join("first");
            let second = root.path().join("second");
            std::fs::create_dir(&first).unwrap();
            std::fs::create_dir(&second).unwrap();
            let db = crate::db::DbManager::new(root.path().join("db.sqlite")).unwrap();
            db.init().unwrap();
            let store = crate::runtime::SessionStore::new(std::sync::Arc::new(db));
            let parent = store.create(&first, "model", "provider").unwrap();
            let member = store.create(&first, "model", "provider").unwrap();
            let sibling = store.create(&first, "model", "provider").unwrap();
            Self {
                workspaces: root.path().join("workspaces"),
                _root: root,
                first: first.canonicalize().unwrap(),
                second: second.canonicalize().unwrap(),
                store,
                parent,
                member,
                sibling,
            }
        }

        fn resolve(&self, id: &str) -> PathBuf {
            self.store
                .db
                .with_conn(|conn| resolve_workspace(conn, &self.workspaces, id, None))
                .unwrap()
                .unwrap()
                .0
        }

        fn project_id(&self, id: &str) -> String {
            self.store
                .db
                .with_conn(|conn| -> Result<String> {
                    Ok(conn.query_row(
                        "SELECT project_id FROM conversations WHERE id=?1",
                        [id],
                        |row| row.get::<_, String>(0),
                    )?)
                })
                .unwrap()
                .unwrap()
        }

        fn project(&self, id: &str) -> project_repo::ProjectRow {
            self.store
                .db
                .with_conn(|conn| -> Result<Option<project_repo::ProjectRow>> {
                    project_repo::get_project(conn, id)
                })
                .unwrap()
                .unwrap()
                .expect("project")
        }

        fn conversation(&self, id: &str) -> (Option<String>, Option<String>) {
            self.store
                .db
                .with_conn(|conn| -> Result<(Option<String>, Option<String>)> {
                    Ok(conn.query_row(
                        "SELECT project_id, workspace_path FROM conversations WHERE id=?1",
                        [id],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )?)
                })
                .unwrap()
                .unwrap()
        }
    }

    #[test]
    fn host_workspace_rebind_moves_only_the_selected_session() {
        let fixture = RebindFixture::new();
        let shared = fixture.project_id(&fixture.member);
        assert_eq!(fixture.project_id(&fixture.sibling), shared);
        fixture
            .store
            .db
            .with_conn(|conn| -> Result<()> {
                conn.execute(
                    "UPDATE projects SET instructions=?1 WHERE id=?2",
                    params!["keep these project instructions", shared],
                )?;
                Ok(())
            })
            .unwrap()
            .unwrap();

        assert_eq!(fixture.resolve(&fixture.parent), fixture.first);
        assert_eq!(fixture.resolve(&fixture.member), fixture.first);
        assert_eq!(fixture.resolve(&fixture.sibling), fixture.first);

        fixture
            .store
            .db
            .with_conn(|conn| rebind_conversation_workspace(conn, &fixture.member, &fixture.second))
            .unwrap()
            .unwrap();

        // The real resolver is authoritative: only the member moved.
        assert_eq!(fixture.resolve(&fixture.member), fixture.second);
        assert_eq!(fixture.resolve(&fixture.parent), fixture.first);
        assert_eq!(fixture.resolve(&fixture.sibling), fixture.first);
        assert_eq!(fixture.project_id(&fixture.sibling), shared);
        assert_eq!(fixture.project_id(&fixture.parent), shared);

        // The new binding keeps project instructions and trusts only the
        // directory the host authorized.
        let moved = fixture.project(&fixture.project_id(&fixture.member));
        assert_eq!(
            moved.instructions.as_deref(),
            Some("keep these project instructions")
        );
        assert_eq!(moved.folders.len(), 1);
        assert!(moved.folders[0].trusted);
        assert_eq!(
            std::fs::canonicalize(&moved.folders[0].path).unwrap(),
            fixture.second
        );
        let original = fixture.project(&shared);
        assert_eq!(
            original.instructions.as_deref(),
            Some("keep these project instructions")
        );
        assert_eq!(original.folders.len(), 1);
        assert!(original.folders[0].trusted);
        assert_eq!(
            std::fs::canonicalize(&original.folders[0].path).unwrap(),
            fixture.first
        );
    }

    #[test]
    fn authorized_move_drops_the_saved_shell_cwd_after_commit() {
        let fixture = RebindFixture::new();
        let shell = fixture._root.path().join("shell-cwd");
        std::fs::create_dir_all(&shell).unwrap();
        std::fs::write(
            shell.join(&fixture.member),
            fixture.first.to_string_lossy().as_ref(),
        )
        .unwrap();
        std::fs::write(shell.join(&fixture.sibling), "/other/workspace").unwrap();

        fixture
            .store
            .db
            .with_conn(|conn| {
                move_conversation_workspace(
                    conn,
                    fixture._root.path(),
                    &fixture.member,
                    &fixture.second,
                )
            })
            .unwrap()
            .unwrap();

        assert!(
            !shell.join(&fixture.member).exists(),
            "the stale shell cwd survived the move"
        );
        assert_eq!(
            std::fs::read_to_string(shell.join(&fixture.sibling)).unwrap(),
            "/other/workspace",
            "another session's saved cwd must not be cleared"
        );
        assert_eq!(fixture.resolve(&fixture.member), fixture.second);
    }

    #[test]
    fn a_failed_authorized_move_restores_the_saved_shell_cwd() {
        let fixture = RebindFixture::new();
        let shell = fixture._root.path().join("shell-cwd");
        std::fs::create_dir_all(&shell).unwrap();
        let original = fixture.first.to_string_lossy().into_owned();
        std::fs::write(shell.join(&fixture.member), &original).unwrap();
        // A managed worktree makes the database rebind fail after the shell
        // state was already staged.
        let project = fixture.project_id(&fixture.member);
        fixture
            .store
            .db
            .with_conn(|conn| -> Result<()> {
                conn.execute(
                    "INSERT INTO conversation_worktrees(conversation_id,project_id,source_path,repo_path,path,branch) VALUES(?1,?2,?3,?4,?5,?6)",
                    params![
                        fixture.member,
                        project.clone(),
                        original,
                        original,
                        original,
                        "agent/test"
                    ],
                )?;
                Ok(())
            })
            .unwrap()
            .unwrap();

        let result = fixture
            .store
            .db
            .with_conn(|conn| {
                move_conversation_workspace(
                    conn,
                    fixture._root.path(),
                    &fixture.member,
                    &fixture.second,
                )
            })
            .unwrap();
        assert!(result.is_err(), "the database rebind should fail");
        assert_eq!(
            std::fs::read_to_string(shell.join(&fixture.member)).unwrap(),
            original,
            "the previous shell cwd must be restored"
        );
        let leftovers: Vec<String> = std::fs::read_dir(&shell)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".stale-"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "staged files left behind: {leftovers:?}"
        );
        // The conversation binding is unchanged; `resolve_workspace` would
        // reject the deliberately fake managed-worktree row.
        assert_eq!(fixture.project_id(&fixture.member), project);
        assert_eq!(
            fixture.conversation(&fixture.member).1.as_deref(),
            Some(original.as_str())
        );
    }

    #[test]
    fn a_shell_listing_failure_leaves_the_database_untouched() {
        let fixture = RebindFixture::new();
        let shell = fixture._root.path().join("shell-cwd");
        // A non-directory makes the real listing fail before any mutation.
        std::fs::write(&shell, "not a directory").unwrap();
        let before = fixture.conversation(&fixture.member);

        let result = fixture
            .store
            .db
            .with_conn(|conn| {
                move_conversation_workspace(
                    conn,
                    fixture._root.path(),
                    &fixture.member,
                    &fixture.second,
                )
            })
            .unwrap();
        let error = result
            .err()
            .expect("a shell listing failure must abort the move");
        let text = format!("{error:#}");
        assert!(
            text.contains("Cannot list the shell state directory"),
            "{text}"
        );
        assert_eq!(fixture.conversation(&fixture.member), before);
        assert_eq!(std::fs::read_to_string(&shell).unwrap(), "not a directory");
        assert_eq!(fixture.resolve(&fixture.member), fixture.first);
    }

    #[test]
    fn a_commit_cleanup_failure_keeps_the_database_move() {
        let fixture = RebindFixture::new();
        let shell = fixture._root.path().join("shell-cwd");
        std::fs::create_dir_all(&shell).unwrap();
        std::fs::write(shell.join(&fixture.member), "/old/review-1").unwrap();
        std::fs::write(shell.join(&fixture.sibling), "/other/workspace").unwrap();
        let fs = crate::native_engine::tool_loop::shell_cwd_faults::FaultyFs::default()
            .failing_removals();

        let result = fixture
            .store
            .db
            .with_conn(|conn| {
                move_conversation_workspace_with(
                    conn,
                    fixture._root.path(),
                    &fixture.member,
                    &fixture.second,
                    &fs,
                )
            })
            .unwrap();
        let error = result
            .err()
            .expect("the staged shell state cannot be removed");
        let text = format!("{error:#}");
        assert!(text.contains("workspace move was committed"), "{text}");

        // The database move stands, the active cache is not restored, and the
        // exact previous bytes remain in the reported backup.
        assert_eq!(fixture.resolve(&fixture.member), fixture.second);
        assert!(!shell.join(&fixture.member).exists());
        let leftovers: Vec<String> = std::fs::read_dir(&shell)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".stale-"))
            .collect();
        assert_eq!(leftovers.len(), 1, "{leftovers:?}");
        assert!(text.contains(&leftovers[0]), "{text}");
        assert_eq!(
            std::fs::read_to_string(shell.join(&leftovers[0])).unwrap(),
            "/old/review-1"
        );
        assert_eq!(
            std::fs::read_to_string(shell.join(&fixture.sibling)).unwrap(),
            "/other/workspace",
            "another session's saved cwd is untouched"
        );
    }

    #[test]
    fn host_workspace_rebind_refuses_a_managed_worktree_session() {
        let fixture = RebindFixture::new();
        let project = fixture.project_id(&fixture.member);
        fixture
            .store
            .db
            .with_conn(|conn| -> Result<()> {
                conn.execute(
                    "INSERT INTO conversation_worktrees(conversation_id,project_id,source_path,repo_path,path,branch) VALUES(?1,?2,?3,?4,?5,?6)",
                    params![
                        fixture.member,
                        project,
                        fixture.first.to_string_lossy(),
                        fixture.first.to_string_lossy(),
                        fixture.first.to_string_lossy(),
                        "agent/test"
                    ],
                )?;
                Ok(())
            })
            .unwrap()
            .unwrap();
        let error = fixture
            .store
            .db
            .with_conn(|conn| rebind_conversation_workspace(conn, &fixture.member, &fixture.second))
            .unwrap()
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("managed worktree"),
            "{error:#}"
        );
        let (project_after, workspace) = fixture.conversation(&fixture.member);
        assert_eq!(project_after.as_deref(), Some(project.as_str()));
        assert_eq!(
            std::fs::canonicalize(workspace.unwrap()).unwrap(),
            fixture.first
        );
    }

    #[test]
    fn host_workspace_rebind_rolls_back_a_mid_transaction_failure() {
        let fixture = RebindFixture::new();
        let (project_before, workspace_before) = fixture.conversation(&fixture.member);
        let count = |table: &str| -> i64 {
            fixture
                .store
                .db
                .with_conn(|conn| -> Result<i64> {
                    Ok(
                        conn.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                            row.get(0)
                        })?,
                    )
                })
                .unwrap()
                .unwrap()
        };
        let projects_before = count("projects");
        let folders_before = count("project_folders");
        // Fail after the project, folder and trust rows were written but
        // before the transaction commits.
        fixture
            .store
            .db
            .with_conn(|conn| -> Result<()> {
                conn.execute_batch(&format!(
                    "CREATE TRIGGER fail_conversation_update BEFORE UPDATE ON conversations \
                     WHEN NEW.id='{}' BEGIN SELECT RAISE(ABORT, 'forced failure'); END;",
                    fixture.member
                ))?;
                Ok(())
            })
            .unwrap()
            .unwrap();
        let result = fixture
            .store
            .db
            .with_conn(|conn| rebind_conversation_workspace(conn, &fixture.member, &fixture.second))
            .unwrap();
        assert!(result.is_err(), "the forced failure must propagate");
        assert_eq!(
            count("projects"),
            projects_before,
            "a partial project was committed"
        );
        assert_eq!(
            count("project_folders"),
            folders_before,
            "partial folders were committed"
        );
        assert_eq!(
            fixture.conversation(&fixture.member),
            (project_before, workspace_before)
        );
        assert_eq!(fixture.resolve(&fixture.member), fixture.first);
    }
}
