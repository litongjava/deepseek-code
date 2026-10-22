//! Explicit hook-backed worktree lifecycle shared by CLI and Desktop.
//! Hook paths are retained by default; removal never recursively deletes in Rust.
use crate::{
    db::DbManager,
    services::tool_hooks::{HookEvent, HookRunner},
};
use anyhow::{ensure, Context, Result};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    time::SystemTime,
};

fn runner(root: &Path, source: &Path) -> Result<HookRunner> {
    let context = crate::services::ServiceContext::open(
        crate::runtime::RuntimePaths::new(root.to_path_buf())?,
        std::sync::Arc::new(crate::services::EnvironmentCredentials),
    )?;
    HookRunner::load_with_plugins(&context, source)
}

pub fn configured(root: &Path, source: &Path) -> Result<bool> {
    Ok(runner(root, source)?
        .list()
        .iter()
        .any(|hook| hook.event == HookEvent::WorktreeCreate))
}
fn provenance(runner: &HookRunner) -> Result<Value> {
    let hooks = runner
        .list()
        .iter()
        .filter(|h| {
            matches!(
                h.event,
                HookEvent::WorktreeCreate | HookEvent::WorktreeRemove
            )
        })
        .collect::<Vec<_>>();
    let digest = ring::digest::digest(&ring::digest::SHA256, &serde_json::to_vec(&hooks)?);
    Ok(
        json!({"sources":hooks.iter().map(|h|h.source.clone()).collect::<Vec<_>>(),"digest":digest.as_ref().iter().map(|b|format!("{b:02x}")).collect::<String>()}),
    )
}
fn created(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .ok()?
        .created()
        .ok()?
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_nanos()
        .try_into()
        .ok()
}
fn validate_path(source: &Path, path: &str) -> Result<PathBuf> {
    let raw = Path::new(path);
    ensure!(
        raw.is_absolute() && raw.is_dir(),
        "WorktreeCreate must return an existing absolute directory"
    );
    let path = raw.canonicalize()?;
    let source = source.canonicalize()?;
    ensure!(
        path.parent().is_some() && path != source && !source.starts_with(&path),
        "WorktreeCreate cannot select the source, its ancestors, or a filesystem root"
    );
    Ok(path)
}
/// Fail closed for cleanup on large, unreadable or changing trees. Symlinks are not followed.
fn bounded_children(path: &Path, remaining: usize) -> Result<Vec<PathBuf>> {
    let children = std::fs::read_dir(path)?
        .take(remaining.saturating_add(1))
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    ensure!(
        children.len() <= remaining,
        "Worktree exceeds the cleanup scan budget (20000 entries)"
    );
    Ok(children)
}

fn fingerprint(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
    let mut pending = vec![path.to_path_buf()];
    let (mut entries, mut bytes) = (0usize, 0u64);
    while let Some(current) = pending.pop() {
        entries += 1;
        ensure!(
            entries <= 20_000,
            "Worktree exceeds the cleanup scan budget (20000 entries)"
        );
        let relative = current.strip_prefix(path)?;
        hash.update(relative.to_string_lossy().as_bytes());
        hash.update(&[0]);
        let metadata = std::fs::symlink_metadata(&current)?;
        if metadata.file_type().is_symlink() {
            hash.update(b"link");
            hash.update(std::fs::read_link(&current)?.to_string_lossy().as_bytes());
        } else if metadata.is_dir() {
            hash.update(b"dir");
            let mut children = bounded_children(&current, 20_000 - entries - pending.len())?;
            children.sort();
            pending.extend(children);
        } else {
            ensure!(metadata.is_file(), "Unsupported special file in worktree");
            hash.update(b"file");
            bytes = bytes
                .checked_add(metadata.len())
                .context("Worktree size overflow")?;
            ensure!(
                bytes <= 128 * 1024 * 1024,
                "Worktree exceeds the cleanup scan budget (128 MiB)"
            );
            let mut file = std::fs::File::open(&current)?.take(metadata.len() + 1);
            let mut buffer = [0u8; 8192];
            let mut read = 0;
            loop {
                let size = file.read(&mut buffer)?;
                if size == 0 {
                    break;
                }
                read += size as u64;
                hash.update(&buffer[..size]);
            }
            ensure!(
                read == metadata.len(),
                "Worktree changed during cleanup scan"
            );
        }
        hash.update(&[0]);
    }
    Ok(hash
        .finish()
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

pub async fn create(
    root: &Path,
    db: &DbManager,
    session: &str,
    source: &Path,
    name: Option<&str>,
) -> Result<Option<PathBuf>> {
    if let Some(name) = name {
        super::session::validate_name(name)?;
    }
    let runner = runner(root, source)?;
    if !runner
        .list()
        .iter()
        .any(|hook| hook.event == HookEvent::WorktreeCreate)
    {
        return Ok(None);
    }
    let project = db
        .with_conn(|conn| crate::native_engine::workspace::conversation_project(conn, session))??
        .context("Worktree requires a trusted project")?;
    ensure!(
        Path::new(
            project
                .workspace_path
                .as_deref()
                .context("Project source missing")?
        )
        .canonicalize()?
            == source.canonicalize()?,
        "Worktree source does not match the trusted project"
    );
    let name = name
        .map(str::to_owned)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let started = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)?
        .as_nanos();
    let output = runner
        .run_worktree(HookEvent::WorktreeCreate, session, json!({"name":name}))
        .await?
        .context("WorktreeCreate path missing")?;
    let path = validate_path(source, &output)?;
    let created_ns = created(&path);
    let newly_created = created_ns.is_some_and(|time| u128::from(time) >= started);
    let baseline = fingerprint(&path).ok();
    let metadata = json!({"canonical_path":path,"created_ns":created_ns,"newly_created":newly_created,"baseline":baseline,"provenance":provenance(&runner)?});
    // A failed DB transaction deliberately retains hook output rather than deleting unknown files.
    db.with_conn(|conn|->Result<()> {
        let tx=crate::db::write_transaction(&conn)?;
        tx.execute("INSERT INTO conversation_worktrees(conversation_id,project_id,source_path,repo_path,path,branch) VALUES(?1,?2,?3,?3,?4,?5)",params![session,project.id,source.to_string_lossy(),path.to_string_lossy(),name])?;
        tx.execute("INSERT INTO conversation_worktree_backends(conversation_id,backend,metadata) VALUES(?1,'hook',?2)",params![session,metadata.to_string()])?;
        tx.execute("UPDATE conversations SET workspace_path=?1 WHERE id=?2",params![path.to_string_lossy(),session])?;
        tx.commit()?; Ok(())
    })??;
    Ok(Some(path))
}

pub(super) fn resolve(conn: &rusqlite::Connection, session: &str, path: &Path) -> Result<bool> {
    let metadata:Option<String>=conn.query_row("SELECT metadata FROM conversation_worktree_backends WHERE conversation_id=?1 AND backend='hook'",[session],|row|row.get(0)).optional()?;
    let Some(metadata) = metadata else {
        return Ok(false);
    };
    let metadata: Value = serde_json::from_str(&metadata)?;
    ensure!(
        path.canonicalize()?
            == PathBuf::from(
                metadata["canonical_path"]
                    .as_str()
                    .context("Hook path missing")?
            ),
        "Hook worktree path identity changed"
    );
    if let Some(expected) = metadata["created_ns"].as_u64() {
        ensure!(
            created(path) == Some(expected),
            "Hook worktree directory was replaced"
        );
    }
    Ok(true)
}

pub fn status(db: &DbManager, session: &str) -> Result<Option<Value>> {
    db.with_conn(|conn| {
        conn.query_row("SELECT w.source_path,w.path,w.branch,COALESCE(b.backend,'git') FROM conversation_worktrees w LEFT JOIN conversation_worktree_backends b USING(conversation_id) WHERE w.conversation_id=?1",[session],|row|Ok(json!({"source":row.get::<_,String>(0)?,"path":row.get::<_,String>(1)?,"name":row.get::<_,String>(2)?,"backend":row.get::<_,String>(3)?}))).optional().map_err(Into::into)
    })?
}

/// Read-only preflight: run before shutting down an idle worker, then recheck at release.
pub fn ensure_idle(root: &Path, session: &str) -> Result<()> {
    ensure!(
        !crate::services::background_sessions::list(root, None)?
            .iter()
            .any(|job| job.session_id == session
                && matches!(job.status.as_str(), "starting" | "running" | "unresponsive")),
        "Stop active session tasks before changing worktree binding"
    );
    let tasks = crate::tools::tasks::execute_for_session("TaskList", json!({}), root, session)?;
    ensure!(
        !tasks["tasks"].as_array().is_some_and(|tasks| tasks
            .iter()
            .any(|task| task["metadata"]["managed_shell"] == true && task["status"] == "running")),
        "Stop managed shell tasks before changing worktree binding"
    );
    Ok(())
}

pub async fn release(root: &Path, db: &DbManager, session: &str, remove: bool) -> Result<PathBuf> {
    ensure_idle(root, session)?;
    let state = status(db, session)?.context("This session has no managed worktree")?;
    let source = PathBuf::from(state["source"].as_str().unwrap());
    let path = PathBuf::from(state["path"].as_str().unwrap());
    ensure!(source.is_dir(), "Original source directory is unavailable");
    if remove {
        db.with_conn(|conn| -> Result<()> {
            let project = crate::native_engine::workspace::conversation_project(conn, session)?
                .context("Worktree requires trusted project")?;
            super::session::resolve(conn, session, &project.id, &source)?
                .context("Worktree binding missing")?;
            Ok(())
        })??;
        let shared: i64 = db.with_conn(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM conversation_worktrees WHERE path=?1",
                [path.to_string_lossy().as_ref()],
                |row| row.get(0),
            )
        })??;
        ensure!(
            shared == 1,
            "Other sessions share this worktree; detach those bindings before removal"
        );
        if state["backend"] == "hook" {
            let metadata: String = db.with_conn(|conn| {
                conn.query_row(
                    "SELECT metadata FROM conversation_worktree_backends WHERE conversation_id=?1",
                    [session],
                    |row| row.get(0),
                )
            })??;
            let metadata: Value = serde_json::from_str(&metadata)?;
            ensure!(
                metadata["newly_created"] == true,
                "Cannot prove this hook created the directory; retain it or use detach"
            );
            db.with_conn(|conn| resolve(conn, session, &path))??;
            let initial = metadata["baseline"].as_str().context(
                "Initial cleanup scan was unavailable or exceeded budget; retain the worktree",
            )?;
            ensure!(
                fingerprint(&path)? == initial,
                "Worktree has changed; retain your changes or use detach"
            );
            let runner = runner(root, &source)?;
            ensure!(
                provenance(&runner)? == metadata["provenance"],
                "Worktree hook configuration changed; retain it or use detach"
            );
            ensure!(
                runner
                    .list()
                    .iter()
                    .any(|h| h.event == HookEvent::WorktreeRemove),
                "No WorktreeRemove hook is configured; use detach to retain files"
            );
            runner
                .run_worktree(
                    HookEvent::WorktreeRemove,
                    session,
                    json!({"worktree_path":path}),
                )
                .await?;
            ensure!(
                !path.exists(),
                "WorktreeRemove hook left the directory in place; binding retained"
            );
        } else {
            ensure!(
                super::session::git(
                    &path,
                    &[
                        "status",
                        "--porcelain",
                        "--untracked-files=all",
                        "--ignored"
                    ]
                )?
                .is_empty(),
                "Worktree contains local or ignored files; retain it or use detach"
            );
            let repo = super::session::git(&source, &["rev-parse", "--show-toplevel"])?;
            let target = super::session::git(&path, &["rev-parse", "--show-toplevel"])?;
            super::session::git(Path::new(&repo), &["worktree", "remove", &target])?;
            // Retain the branch: committed work is never discarded by cleanup.
        }
    }
    db.with_conn(|conn| -> Result<()> {
        let tx = crate::db::write_transaction(&conn)?;
        tx.execute(
            "DELETE FROM conversation_worktree_backends WHERE conversation_id=?1",
            [session],
        )?;
        tx.execute(
            "DELETE FROM conversation_worktrees WHERE conversation_id=?1",
            [session],
        )?;
        tx.execute(
            "UPDATE conversations SET workspace_path=?1 WHERE id=?2",
            params![source.to_string_lossy(), session],
        )?;
        tx.commit()?;
        Ok(())
    })??;
    Ok(source)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{RuntimePaths, SessionStore};
    #[test]
    fn wide_directory_is_rejected_before_unbounded_collection() {
        let temp = tempfile::tempdir().unwrap();
        for name in ["one", "two", "three"] {
            std::fs::write(temp.path().join(name), "fixture").unwrap();
        }
        assert!(bounded_children(temp.path(), 2).is_err());
        assert_eq!(bounded_children(temp.path(), 3).unwrap().len(), 3);
        assert!(bounded_children(temp.path(), 0).is_err());
    }
    #[test]
    fn idle_preflight_refuses_shell_without_cancelling_or_changing_record() {
        let (_temp, paths, store, source) = fixture();
        let session = store.create(&source, "fixture", "fixture").unwrap();
        let task = crate::tools::tasks::execute_for_shell_owner(
            "TaskCreate",
            json!({"subject":"server","metadata":{"managed_shell":true}}),
            &paths.data_dir,
            &session,
        )
        .unwrap();
        let id = task["id"].as_str().unwrap();
        crate::tools::tasks::execute_for_shell_owner(
            "TaskUpdate",
            json!({"id":id,"status":"running"}),
            &paths.data_dir,
            &session,
        )
        .unwrap();
        assert!(ensure_idle(&paths.data_dir, &session).is_err());
        let task = crate::tools::tasks::execute_for_session(
            "TaskGet",
            json!({"id":id}),
            &paths.data_dir,
            &session,
        )
        .unwrap();
        assert_eq!(task["status"], "running");
        assert!(!paths
            .data_dir
            .join("shell-tasks")
            .join(&session)
            .join(id)
            .join("cancel")
            .exists());
    }

    #[tokio::test]
    async fn installed_plugin_hooks_share_discovery_creation_and_cleanup() {
        let (temp, paths, store, source) = fixture();
        std::fs::remove_file(source.join(".claude/settings.json")).unwrap();
        let script = std::fs::read_to_string(temp.path().join("hook.cjs")).unwrap();
        let command = "node \"${CLAUDE_PLUGIN_ROOT}/worktree.cjs\"";
        let bundle = json!({"id":"worktree-fixture","version":"1.0.0","description":"fixture","commands":{},"files":{"worktree.cjs":script},"lifecycle_hooks":{"WorktreeCreate":[{"hooks":[{"type":"command","command":command}]}],"WorktreeRemove":[{"hooks":[{"type":"command","command":command}]}]}});
        let bundle_path = temp.path().join("plugin.json");
        std::fs::write(&bundle_path, bundle.to_string()).unwrap();
        let context = crate::services::ServiceContext::open(
            paths.clone(),
            std::sync::Arc::new(crate::services::EnvironmentCredentials),
        )
        .unwrap();
        crate::services::plugins::install(&context, bundle_path.to_str().unwrap(), None)
            .await
            .unwrap();
        assert!(configured(&paths.data_dir, &source).unwrap());
        let session = store.create(&source, "fixture", "fixture").unwrap();
        let path = create(
            &paths.data_dir,
            &store.db,
            &session,
            &source,
            Some("plugin"),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(path.join("initial.txt").exists());
        release(&paths.data_dir, &store.db, &session, true)
            .await
            .unwrap();
        assert!(!path.exists());
        crate::services::plugins::enable(&context, "worktree-fixture", false).unwrap();
        assert!(!configured(&paths.data_dir, &source).unwrap());
    }
    fn fixture() -> (tempfile::TempDir, RuntimePaths, SessionStore, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        std::fs::create_dir_all(source.join(".claude")).unwrap();
        let script = temp.path().join("hook.cjs");
        let code = r#"const fs=require('fs'),path=require('path');let s='';process.stdin.on('data',d=>s+=d);process.stdin.on('end',()=>{let v=JSON.parse(s);if(v.hook_event_name==='WorktreeCreate'){let p=path.join(path.dirname(v.cwd),'created-'+v.name);fs.mkdirSync(p);fs.writeFileSync(path.join(p,'initial.txt'),'initial');process.stdout.write(p);}else{fs.rmSync(v.worktree_path,{recursive:true});}});"#;
        std::fs::write(&script, code).unwrap();
        let command = format!("node \"{}\"", script.display());
        std::fs::write(source.join(".claude/settings.json"),json!({"hooks":{"WorktreeCreate":[{"hooks":[{"type":"command","command":command}]}],"WorktreeRemove":[{"hooks":[{"type":"command","command":command}]}]}}).to_string()).unwrap();
        let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
        let store = SessionStore::new(paths.open_database().unwrap());
        (temp, paths, store, source)
    }
    #[tokio::test]
    async fn hook_binding_resumes_forks_preserves_dirty_files_and_explicitly_removes_clean_trees() {
        let (_temp, paths, store, source) = fixture();
        let session = store.create(&source, "fixture", "fixture").unwrap();
        let path = create(&paths.data_dir, &store.db, &session, &source, Some("dirty"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            store
                .workspace(&session, &paths.data_dir.join("workspaces"))
                .unwrap(),
            path
        );
        let fork = store.fork(&session).unwrap();
        assert_eq!(
            store
                .workspace(&fork, &paths.data_dir.join("workspaces"))
                .unwrap(),
            path
        );
        assert!(release(&paths.data_dir, &store.db, &session, true)
            .await
            .unwrap_err()
            .to_string()
            .contains("Other sessions"));
        release(&paths.data_dir, &store.db, &fork, false)
            .await
            .unwrap();
        std::fs::write(path.join("initial.txt"), "valuable change").unwrap();
        assert!(release(&paths.data_dir, &store.db, &session, true)
            .await
            .is_err());
        assert_eq!(
            std::fs::read_to_string(path.join("initial.txt")).unwrap(),
            "valuable change"
        );
        release(&paths.data_dir, &store.db, &session, false)
            .await
            .unwrap();
        assert!(path.exists());
        assert!(status(&store.db, &session).unwrap().is_none());
        let clean = store.create(&source, "fixture", "fixture").unwrap();
        let path = create(&paths.data_dir, &store.db, &clean, &source, Some("clean"))
            .await
            .unwrap()
            .unwrap();
        if created(&path).is_some() {
            release(&paths.data_dir, &store.db, &clean, true)
                .await
                .unwrap();
            assert!(!path.exists());
            assert_eq!(
                store
                    .workspace(&clean, &paths.data_dir.join("workspaces"))
                    .unwrap()
                    .canonicalize()
                    .unwrap(),
                source.canonicalize().unwrap()
            );
        }
    }
    #[test]
    fn returned_paths_and_dirty_snapshot_are_checked() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        std::fs::create_dir(&source).unwrap();
        assert!(validate_path(&source, "relative").is_err());
        assert!(validate_path(&source, source.to_str().unwrap()).is_err());
        assert!(validate_path(&source, temp.path().to_str().unwrap()).is_err());
        assert!(validate_path(&source, source.join("missing").to_str().unwrap()).is_err());
        let before = fingerprint(&source).unwrap();
        std::fs::write(source.join("untracked"), "keep").unwrap();
        assert_ne!(before, fingerprint(&source).unwrap());
    }
    #[tokio::test]
    async fn failed_create_never_binds_a_session() {
        let (_temp, paths, store, source) = fixture();
        std::fs::write(
            source.join(".claude/settings.json"),
            json!({"hooks":{"WorktreeCreate":[{"hooks":[{"type":"command","command":"exit 2"}]}]}})
                .to_string(),
        )
        .unwrap();
        let session = store.create(&source, "fixture", "fixture").unwrap();
        assert!(create(
            &paths.data_dir,
            &store.db,
            &session,
            &source,
            Some("failed")
        )
        .await
        .is_err());
        assert!(status(&store.db, &session).unwrap().is_none());
    }
}
