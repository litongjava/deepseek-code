//! Explicit, session-local repair of saved project trust.
use crate::{
    db::{conversation_repo, project_repo, session_store::SessionAccess},
    native_engine::workspace,
};
use anyhow::{ensure, Context, Result};
use rusqlite::{params, Connection};
use serde::Serialize;
use std::path::{Path, PathBuf};

pub(crate) fn project(conn: &Connection, id: &str) -> Result<project_repo::ProjectRow> {
    let session =
        conversation_repo::conversation_metadata(conn, id)?.context("Session not found")?;
    ensure!(
        session.workspace_mode == "code",
        "Only Code sessions can be trusted in CLI"
    );
    ensure!(!session.archived, "Session is archived");
    let project = project_repo::get_project(
        conn,
        session
            .project_id
            .as_deref()
            .context("Session has no project")?,
    )?
    .context("Project not found")?;
    ensure!(!project.is_archived, "Project is archived");
    Ok(project)
}

/// Unlike resume, inspection must remain usable when trust is invalid.
pub fn inspect(db: &impl SessionAccess, id: &str) -> Result<project_repo::ProjectRow> {
    db.select_session(id)?.inspect_project_trust()
}

#[derive(Debug, Serialize)]
pub struct Repaired {
    pub session_id: String,
    pub project_id: String,
    pub folders: Vec<String>,
    pub workspace: PathBuf,
}

/// Every existing project root must be explicitly selected. This is not a
/// relocation command: missing source folders and unrelated roots are rejected.
pub fn repair(
    db: &impl SessionAccess,
    data_root: &Path,
    id: &str,
    folders: &[PathBuf],
) -> Result<Repaired> {
    let owner = db.select_session(id)?;
    let requested = project_repo::normalize_folders(
        &folders
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
    )?;
    std::fs::create_dir_all(data_root.join("tasks/background"))?;
    // The same exclusive claim used by the worker prevents a concurrent launch.
    // Idle persistent workers also own this claim and must be shut down first.
    let _claim = super::background_sessions::SessionClaim::acquire(data_root, id, &uuid::Uuid::new_v4().to_string())
        .context("Cannot repair trust while a session worker owns this session; stop its tasks and shut down the worker first")?;
    ensure!(
        !super::background_sessions::possible_executions(data_root, id)?
            .iter()
            .any(|job| job.session_id == id
                && matches!(job.status.as_str(), "starting" | "running" | "unresponsive")),
        "Cannot repair trust while this session has an active task; stop it first"
    );
    owner.repair_project_trust(data_root, &requested)
}

pub(crate) fn repair_in(
    tx: &Connection,
    data_root: &Path,
    id: &str,
    requested: &[String],
) -> Result<Repaired> {
    let original = project(&tx, id)?;
    let original_paths = original
        .folders
        .iter()
        .map(|f| f.path.clone())
        .collect::<Vec<_>>();
    let expected = project_repo::normalize_folders(&original_paths).context(
        "A saved project folder is unavailable; restore that folder before repairing trust",
    )?;
    ensure!(requested.len() == expected.len() && requested.iter().all(|p| expected.contains(p)),
            "Supply every current project folder with --folder; missing or unrelated folders are not accepted. Expected: {}", expected.join("; "));
    let source = original
        .workspace_path
        .as_deref()
        .context("Project has no working directory")?;
    let normalized_source = project_repo::normalize_folders(&[source.to_owned()])?.remove(0);
    let readable_source = source
        .strip_prefix("\\\\?\\UNC\\")
        .map(|p| format!("\\\\{p}"))
        .unwrap_or_else(|| source.strip_prefix("\\\\?\\").unwrap_or(source).to_owned());
    let saved_source = Path::new(&readable_source).components().collect::<Vec<_>>();
    let current_source = Path::new(&normalized_source)
        .components()
        .collect::<Vec<_>>();
    ensure!(saved_source.len() == current_source.len() && saved_source.iter().zip(&current_source).all(|(a,b)| {
            if cfg!(windows) { a.as_os_str().to_string_lossy().eq_ignore_ascii_case(&b.as_os_str().to_string_lossy()) } else { a == b }
        }), "Project source folder changed; restore its original identity or start a new session in the new folder");
    ensure!(
        expected.first() == Some(&normalized_source),
        "Project source is not its primary folder; restore its source identity first"
    );
    let mismatched_mapping: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM conversation_worktrees WHERE conversation_id=?1 AND project_id<>?2)", params![id, original.id], |row| row.get(0))?;
    ensure!(!mismatched_mapping, "Session worktree project identity changed; restore its original mapping before repairing trust");
    // Validate the existing mapping against the ORIGINAL source before
    // copying project metadata. Never replace a source with a worktree path.
    let managed = crate::worktree::session::resolve(&tx, id, &original.id, Path::new(source))?;
    let project_id = uuid::Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    project_repo::insert_project(
        &tx,
        &project_id,
        &original.name,
        original.description.as_deref(),
        original.instructions.as_deref(),
        Some(&normalized_source),
        false,
        &now,
        &now,
    )?;
    project_repo::set_folders(&tx, &project_id, &expected)?;
    project_repo::trust_folders(&tx, &project_id, &expected)?;
    // Preserve attached project references without changing the shared project.
    for file in project_repo::list_project_files(&tx, &original.id)? {
        project_repo::insert_project_file(
            &tx,
            &uuid::Uuid::new_v4().to_string(),
            &project_id,
            file.file_name.as_deref(),
            file.file_path.as_deref(),
            file.file_size,
            file.mime_type.as_deref(),
        )?;
    }
    tx.execute(
        "UPDATE conversations SET project_id=?1, updated_at=?2 WHERE id=?3",
        params![project_id, now, id],
    )?;
    tx.execute(
        "UPDATE conversation_worktrees SET project_id=?1 WHERE conversation_id=?2",
        params![project_id, id],
    )?;
    let (resolved, _) =
        workspace::resolve_workspace(&tx, &data_root.join("data/workspaces"), id, None)?;
    if let Some(managed) = managed {
        ensure!(
            resolved == managed,
            "Trust repair changed the managed workspace"
        );
    }
    tx.execute(
        "UPDATE conversations SET workspace_path=?1 WHERE id=?2",
        params![resolved.to_string_lossy(), id],
    )?;
    Ok(Repaired {
        session_id: id.into(),
        project_id,
        folders: expected,
        workspace: resolved,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{RuntimePaths, SessionStore};

    fn fixture() -> (
        tempfile::TempDir,
        RuntimePaths,
        SessionStore,
        String,
        PathBuf,
    ) {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("中文 source");
        std::fs::create_dir(&source).unwrap();
        let paths = RuntimePaths::new(root.path().join("data")).unwrap();
        let store = SessionStore::new(paths.open_database().unwrap());
        let id = store.create(&source, "model", "provider").unwrap();
        (root, paths, store, id, source)
    }
    fn distrust(store: &SessionStore, id: &str) {
        let project = inspect(&store.db, id).unwrap();
        store
            .db
            .fixture_directory(|conn| {
                conn.execute(
                    "UPDATE project_folders SET trusted=0 WHERE project_id=?1",
                    [&project.id],
                )
            })
            .unwrap()
            .unwrap();
    }

    #[test]
    fn complete_explicit_roots_repair_only_selected_session_and_preserve_history() {
        let (root, paths, store, id, source) = fixture();
        let second = root.path().join("second root");
        std::fs::create_dir(&second).unwrap();
        let other = store.create(&source, "model", "provider").unwrap();
        let original = inspect(&store.db, &id).unwrap();
        store
            .db
            .fixture_directory(|conn| {
                project_repo::set_folders(
                    conn,
                    &original.id,
                    &[
                        source.to_string_lossy().into_owned(),
                        second.to_string_lossy().into_owned(),
                    ],
                )
                .unwrap();
                project_repo::insert_project_file(
                    conn,
                    "attachment",
                    &original.id,
                    Some("notes"),
                    Some("notes.txt"),
                    Some(20),
                    Some("text/plain"),
                )
                .unwrap();
            })
            .unwrap();
        store.append(&id, "user", "keep transcript").unwrap();
        distrust(&store, &id);
        assert!(store.resume(&id).is_err());
        assert!(repair(&store.db, &paths.data_dir, &id, &[source.clone()]).is_err());
        assert!(repair(
            &store.db,
            &paths.data_dir,
            &id,
            &[source.clone(), second.clone(), root.path().to_owned()]
        )
        .is_err());
        assert_eq!(inspect(&store.db, &id).unwrap().id, original.id);
        let fixed = repair(&store.db, &paths.data_dir, &id, &[second, source.clone()]).unwrap();
        assert_ne!(fixed.project_id, original.id);
        assert!(store.resume(&id).is_ok());
        assert_eq!(
            std::fs::canonicalize(fixed.workspace).unwrap(),
            source.canonicalize().unwrap()
        );
        assert_eq!(store.history(&id).unwrap()[0]["content"], "keep transcript");
        assert_eq!(inspect(&store.db, &other).unwrap().id, original.id);
        assert!(store.resume(&other).is_err());
        store
            .db
            .fixture_directory(|conn| {
                assert_eq!(
                    project_repo::list_project_files(conn, &fixed.project_id)
                        .unwrap()
                        .len(),
                    1
                );
                assert_eq!(
                    project_repo::list_project_files(conn, &original.id)
                        .unwrap()
                        .len(),
                    1
                );
            })
            .unwrap();
    }

    #[test]
    fn unavailable_roots_and_source_identity_drift_are_rejected_without_mutation() {
        let (root, paths, store, id, source) = fixture();
        distrust(&store, &id);
        let original = inspect(&store.db, &id).unwrap();
        let moved = root.path().join("moved");
        std::fs::rename(&source, &moved).unwrap();
        assert!(repair(&store.db, &paths.data_dir, &id, &[moved.clone()]).is_err());
        std::fs::rename(&moved, &source).unwrap();
        // A path that now resolves through a different parent cannot redefine
        // the saved source identity, even when explicitly supplied.
        let ambiguous = source.join("..").join(source.file_name().unwrap());
        store
            .db
            .fixture_directory(|conn| {
                conn.execute(
                    "UPDATE projects SET workspace_path=?1 WHERE id=?2",
                    params![ambiguous.to_string_lossy(), original.id],
                )
            })
            .unwrap()
            .unwrap();
        let error = repair(&store.db, &paths.data_dir, &id, &[source]).unwrap_err();
        assert!(format!("{error:#}").contains("source folder changed"));
        assert_eq!(inspect(&store.db, &id).unwrap().id, original.id);
        assert!(!paths
            .data_dir
            .join("tasks/background")
            .join(format!("session-{id}.claim"))
            .exists());
    }

    #[test]
    fn worker_claim_blocks_repair_and_is_never_removed() {
        let (_root, paths, store, id, source) = fixture();
        distrust(&store, &id);
        std::fs::create_dir_all(paths.data_dir.join("tasks/background")).unwrap();
        let owner = super::super::background_sessions::SessionClaim::acquire(
            &paths.data_dir,
            &id,
            "worker-owner",
        )
        .unwrap();
        let error = repair(&store.db, &paths.data_dir, &id, &[source]).unwrap_err();
        assert!(error.to_string().contains("worker owns"));
        assert_eq!(
            std::fs::read_to_string(
                paths
                    .data_dir
                    .join("tasks/background")
                    .join(format!("session-{id}.claim"))
            )
            .unwrap(),
            "worker-owner"
        );
        drop(owner);
    }

    #[test]
    fn changed_secondary_path_requires_full_explicit_selection() {
        let (root, paths, store, id, source) = fixture();
        let second = root.path().join("secondary");
        std::fs::create_dir(&second).unwrap();
        let original = inspect(&store.db, &id).unwrap();
        store
            .db
            .fixture_directory(|conn| {
                project_repo::set_folders(
                    conn,
                    &original.id,
                    &[
                        source.to_string_lossy().into_owned(),
                        second.to_string_lossy().into_owned(),
                    ],
                )
                .unwrap();
                conn.execute(
                    "UPDATE project_folders SET path=?1 WHERE project_id=?2 AND sort_order=1",
                    params![
                        second.join("..").join("secondary").to_string_lossy(),
                        original.id
                    ],
                )
                .unwrap();
            })
            .unwrap();
        assert!(store.resume(&id).is_err());
        repair(&store.db, &paths.data_dir, &id, &[source, second]).unwrap();
        assert!(store.resume(&id).is_ok());
    }

    #[test]
    fn managed_worktree_and_fork_keep_original_source_mapping() {
        let (_root, paths, store, id, source) = fixture();
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .current_dir(&source)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-b", "main"]);
        git(&["config", "user.email", "fixture@example.com"]);
        git(&["config", "user.name", "Fixture"]);
        git(&["commit", "--allow-empty", "-m", "initial"]);
        let prepared =
            crate::worktree::session::prepare_named(&source, Some("trust-test")).unwrap();
        let original = inspect(&store.db, &id).unwrap();
        store
            .db
            .fixture_directory(|conn| prepared.save(conn, &id, &original.id, source.to_str().unwrap()))
            .unwrap()
            .unwrap();
        let fork = store.fork(&id).unwrap();
        distrust(&store, &id);
        let fixed = repair(&store.db, &paths.data_dir, &fork, &[source.clone()]).unwrap();
        assert_eq!(fixed.workspace, prepared.path);
        assert_eq!(
            store
                .workspace(&fork, &paths.data_dir.join("data/workspaces"))
                .unwrap(),
            prepared.path
        );
        assert!(store.resume(&id).is_err());
        store
            .db
            .fixture_directory(|conn| {
                let source_saved: String = conn
                    .query_row(
                        "SELECT source_path FROM conversation_worktrees WHERE conversation_id=?1",
                        [&fork],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(
                    Path::new(&source_saved).canonicalize().unwrap(),
                    source.canonicalize().unwrap()
                );
            })
            .unwrap();
        prepared.cleanup();
    }
}
