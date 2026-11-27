use anyhow::Result;
use deepseek_code_core::{db::session_store::Stores, services::background_sessions as jobs};
use serde_json::json;
use std::{fs, path::Path};
use uuid::Uuid;

fn entry(
    root: &Path,
    owner: &str,
    incarnation: &str,
    launched: bool,
) -> Result<std::path::PathBuf> {
    let job = Uuid::new_v4().to_string();
    let dir = root.join("tasks/background").join(&job);
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("state.json"),json!({"id":job,"session_id":owner,"parent_session_id":owner,
        "launched":launched,"title":"fixture","status":"completed","started_ms":1,"heartbeat_ms":1,"finished_ms":2}).to_string())?;
    fs::write(
        dir.join("session-owners.json"),
        json!({"parent":incarnation,"execution":incarnation}).to_string(),
    )?;
    Ok(dir)
}
#[test]
fn job_participants() -> Result<()> {
    let root = tempfile::tempdir()?;
    let db = Stores::open(root.path())?;
    let mine = db.create("mine", Default::default())?;
    let other = db.create("other", Default::default())?;
    entry(root.path(), mine.id(), mine.incarnation(), true)?;
    let dir = entry(root.path(), other.id(), other.incarnation(), false)?;
    fs::write(dir.join("worker.claim"), [0xff])?;
    let filtered = jobs::list(root.path(), Some(mine.id()))?;
    let scoped = jobs::owned_jobs(root.path(), mine.id());
    println!(
        "parent-filtered listing={} own job(s); owned_jobs={:?}",
        filtered.len(),
        scoped.as_ref().map(Vec::len)
    );
    anyhow::ensure!(
        filtered.len() == 1
            && scoped?.len() == 1
            && jobs::executing_in(root.path(), mine.id())?.len() == 1
    );
    let scoped_store = mine.scoped_stores();
    let address = mine.address();
    mine.delete()?;
    let replacement = db.create("mine", Default::default())?;
    assert!(scoped_store.open_session(replacement.id()).is_err());
    assert!(db.resume_history(&address).is_err());
    assert!(jobs::owned_jobs(root.path(), replacement.id())?.is_empty());
    Ok(())
}
