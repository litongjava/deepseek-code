use anyhow::Result;
use deepseek_code_core::{
    db::{conversation_repo::WorkspaceMode, session_store::Stores},
    runtime::SessionStore,
};
use std::{path::Path, process::Command, sync::Arc};

#[test]
fn fork_and_clear_remain_visible_in_the_worktree_picker() -> Result<()> {
    // Use a fresh process for the upgrade so the resolver cannot reuse the
    // in-process root that populated the fixture.
    if let Some(root) = std::env::var_os("I46_WORKTREE_MIGRATION_ROOT") {
        let db = Stores::open(Path::new(&root))?;
        for id in ["I46_FORK_ID", "I46_CLEAR_ID"] {
            let id = std::env::var(id)?;
            let path = db.open_session(&id)?.workspace()?;
            assert!(db
                .resume_entries("none", Some(&path))?
                .iter()
                .any(|entry| entry.conversation.id == id));
        }
        return Ok(());
    }
    let root = tempfile::tempdir()?;
    let project = root.path().join("project");
    std::fs::create_dir(&project)?;
    for args in [
        vec!["init", "-q"],
        vec![
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgSign=false",
            "commit",
            "--allow-empty",
            "-qm",
            "fixture",
        ],
    ] {
        assert!(Command::new("git")
            .current_dir(&project)
            .args(args)
            .status()?
            .success());
    }
    let db = Stores::open(root.path())?;
    let sessions = SessionStore::new(Arc::new(db.clone()));
    let seed = sessions.create(&project, "model", "provider")?;
    let project_id = sessions.resume(&seed)?.project_id.unwrap();
    let source = db.create_project_chat(
        &project_id,
        Some("CLI session"),
        Some("model"),
        WorkspaceMode::Code,
        None,
        true,
    )?;
    sessions.append(&source.id, "user", "Pick this worktree")?;
    let path = sessions.workspace(&source.id, &root.path().join("data/workspaces"))?;
    let fork = sessions.fork(&source.id)?;
    let clear = sessions.clear(&source.id)?;
    sessions.append(&clear, "user", "New session")?;
    for id in [&fork, &clear] {
        assert_eq!(
            sessions
                .resume(id)?
                .workspace_path
                .as_deref()
                .map(Path::new),
            Some(path.as_path())
        );
        assert!(db
            .resume_entries("none", Some(&path))?
            .iter()
            .any(|r| r.conversation.id == *id));
    }
    drop((sessions, db));
    let conn = rusqlite::Connection::open(root.path().join("global.db"))?;
    conn.execute(
        "UPDATE conversations SET workspace_path=?1 WHERE id IN (?2,?3)",
        rusqlite::params![project.to_str(), fork, clear],
    )?;
    conn.pragma_update(None, "user_version", 14)?;
    drop(conn);
    let upgrade = Command::new(std::env::current_exe()?)
        .args([
            "--exact",
            "fork_and_clear_remain_visible_in_the_worktree_picker",
        ])
        .env("I46_WORKTREE_MIGRATION_ROOT", root.path())
        .env("I46_FORK_ID", fork)
        .env("I46_CLEAR_ID", clear)
        .status()?;
    assert!(upgrade.success());
    Ok(())
}

#[test]
fn continue_selects_the_greater_id_on_tied_activity() -> Result<()> {
    let root = tempfile::tempdir()?;
    let db = Stores::open(root.path())?;
    let sessions = SessionStore::new(Arc::new(db));
    let ids = [
        "00000000-0000-4000-8000-000000000001",
        "00000000-0000-4000-8000-000000000002",
    ];
    for id in ids {
        sessions.create_with_id(root.path(), "model", "provider", id)?;
        sessions.append(id, "user", "tie")?;
    }
    rusqlite::Connection::open(root.path().join("global.db"))?.execute(
        "UPDATE conversations SET updated_at='2030-01-01T00:00:00Z' WHERE id IN (?1,?2)",
        ids,
    )?;
    assert_eq!(sessions.latest(root.path())?.unwrap().id, ids[1]);
    Ok(())
}

#[test]
fn default_fork_title_uses_the_displayed_source_title() -> Result<()> {
    use deepseek_code_core::db::session_store::{Branch, CreateOptions};
    let root = tempfile::tempdir()?;
    let db = Stores::open(root.path())?;
    let source = db.create(
        "source",
        CreateOptions {
            title: Some("CLI session".into()),
            ..Default::default()
        },
    )?;
    source.write(|w| w.append("user", "Review the migration"))?;
    source.branch("destination", Branch::Full)?;
    assert_eq!(
        db.conversation_metadata("destination")?
            .unwrap()
            .title
            .as_deref(),
        Some("Review the migration (branch)")
    );
    Ok(())
}

#[test]
fn latest_preview_uses_creation_time_after_import() -> Result<()> {
    let root = tempfile::tempdir()?;
    let db = Stores::open(root.path())?;
    let id = "00000000-0000-4000-8000-000000000019";
    let shard = db.create(id, Default::default())?;
    let first_line = "recent ".repeat(18).trim_end().to_owned();
    shard.write(|w| {
        w.append("user", &format!("{first_line}\nsecond line"))?;
        w.append("user", "last in transcript")?;
        Ok(())
    })?;
    let mut rows = db.sync_messages(id)?;
    rows[0].created_at = "2026-11-20T00:00:00Z".into();
    rows[1].created_at = "2025-11-20T00:00:00Z".into();
    db.sync_conversation(id, None, &rows)?;
    let entry = db.resume_entries("other", None)?.pop().unwrap();
    assert_eq!(entry.latest.as_deref(), Some(first_line.as_str()));
    drop((shard, db));

    let old = rusqlite::Connection::open(
        root.path()
            .join("data/sessions")
            .join(id)
            .join("session.db"),
    )?;
    old.execute_batch(
        "UPDATE projection_state SET latest_prompt='last in transcript'; PRAGMA user_version=9",
    )?;
    drop(old);
    let global = rusqlite::Connection::open(root.path().join("global.db"))?;
    global.execute(
        "UPDATE conversations SET latest_prompt='last in transcript' WHERE id=?1",
        [id],
    )?;
    drop(global);
    let db = Stores::open(root.path())?;
    db.open_session(id)?;
    assert_eq!(
        db.resume_entries("other", None)?
            .pop()
            .unwrap()
            .latest
            .as_deref(),
        Some(first_line.as_str())
    );
    Ok(())
}
