use anyhow::Result;
use deepseek_code_core::{db::session_store::Stores, tools::tasks};
use serde_json::json;
use std::{
    fs,
    time::{Duration, Instant},
};
use uuid::Uuid;
#[tokio::test]
async fn default_task_snapshot() -> Result<()> {
    let root = tempfile::tempdir()?;
    let db = Stores::open(root.path())?;
    let pid = Uuid::new_v4().to_string();
    let cid = Uuid::new_v4().to_string();
    let parent = db.create(&pid, Default::default())?;
    let child = db.create(&cid, Default::default())?;
    child.write(|w| w.append("assistant", "committed report"))?;
    let job = Uuid::new_v4().to_string();
    let dir = root.path().join("tasks/background").join(&job);
    fs::create_dir_all(&dir)?;
    fs::write(
        dir.join("state.json"),
        json!({"id":job,"parent_session_id":pid,"session_id":cid,
        "status":"completed","title":"fixture","started_ms":1,"heartbeat_ms":1})
        .to_string(),
    )?;
    fs::write(
        dir.join("output.jsonl"),
        "{\"type\":\"result\",\"result\":\"saved\"}\n",
    )?;
    fs::write(
        dir.join("session-owners.json"),
        json!({"parent":parent.incarnation(),"execution":child.incarnation()}).to_string(),
    )?;
    let key = fs::File::open(root.path().join("state/locks/session").join(&cid))?;
    key.lock()?;
    let unlock = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1400));
        drop(key)
    });
    let start = Instant::now();
    let result = tasks::execute_for_session_async(
        "TaskGet",
        json!({"task_id":job,"block":false}),
        root.path(),
        &pid,
    )
    .await;
    println!(
        "time={:?}, retrieval={}, stdout={}",
        start.elapsed(),
        result.as_ref().unwrap()["retrieval_status"],
        result.as_ref().unwrap()["stdout"]
    );
    let elapsed = start.elapsed();
    let omitted = Instant::now();
    let default =
        tasks::execute_for_session_async("TaskGet", json!({"task_id":job}), root.path(), &pid)
            .await?;
    let omitted_elapsed = omitted.elapsed();
    unlock.join().unwrap();
    assert!(elapsed < Duration::from_millis(650) && omitted_elapsed < Duration::from_millis(650));
    assert!(["success", "timeout"].contains(&default["retrieval_status"].as_str().unwrap()));
    assert!(
        tasks::execute_for_session_async("TaskGet", json!({"task_id":job}), root.path(), &cid)
            .await
            .is_err()
    );
    Ok(())
}
