use anyhow::Result;
use deepseek_code_core::{
    db::session_store::Stores, runtime::SessionStore, services::background_sessions,
};
use serde_json::json;
use std::{fs, sync::Arc};
use uuid::Uuid;

#[test]
fn retired_owned_child_reports_saved_output_once_and_keeps_following_jobs() -> Result<()> {
    let root = tempfile::tempdir()?;
    let db = Stores::open(root.path())?;
    let store = SessionStore::new(Arc::new(db.clone()));
    let parent = Uuid::new_v4().to_string();
    let owner = db.create(&parent, Default::default())?;
    let child = Uuid::new_v4().to_string();
    let target = db.create(&child, Default::default())?;
    for (i, body) in ["saved child output", "next job result"]
        .into_iter()
        .enumerate()
    {
        let id = Uuid::new_v4().to_string();
        let dir = root.path().join("tasks/background").join(&id);
        fs::create_dir_all(&dir)?;
        fs::write(dir.join("state.json"),json!({"id":id,"parent_session_id":parent,"session_id":if i==0 {&parent}else{&child},"title":"report","status":"completed","started_ms":2-i,"heartbeat_ms":2}).to_string())?;
        fs::write(dir.join("session-owners.json"),json!({"parent":owner.incarnation(),"execution":if i==0 {owner.incarnation()}else{target.incarnation()}}).to_string())?;
        fs::write(
            dir.join("output.jsonl"),
            json!({"type":"result","result":body}).to_string(),
        )?;
        if i == 0 {
            fs::write(dir.join("detached"), "1")?;
            fs::write(dir.join("owned-child.json"), serde_json::to_vec(&child)?)?;
        }
    }
    target.delete()?;
    let notices = background_sessions::collect_completed(&store, root.path(), &parent)?;
    assert_eq!(notices.len(), 2);
    for expected in ["saved child output", "next job result"] {
        assert!(notices.iter().any(|s| s.contains(expected)));
    }
    assert!(background_sessions::collect_completed(&store, root.path(), &parent)?.is_empty());
    owner.delete()?;
    db.create(&parent, Default::default())?;
    assert!(background_sessions::collect_completed(&store, root.path(), &parent)?.is_empty());
    Ok(())
}
