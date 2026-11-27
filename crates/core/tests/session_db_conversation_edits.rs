use anyhow::Result;
use deepseek_code_core::db::session_store::{CreateOptions, Stores};
use serde_json::json;

#[test]
fn combined_edit_preserves_history_on_failure_and_recovers_after_decision() -> Result<()> {
    let root = tempfile::tempdir()?;
    let stores = Stores::open(root.path())?;
    let id = r#"edit"with\escapes"#;
    let shard = stores.create(
        id,
        CreateOptions {
            title: Some("before".into()),
            ..Default::default()
        },
    )?;
    shard.write(|w| w.append("user", "keep me"))?;
    let fault = rusqlite::Connection::open(root.path().join("global.db"))?;
    fault.execute_batch(
        "CREATE TRIGGER reject_title BEFORE UPDATE OF title ON conversations
        BEGIN SELECT RAISE(ABORT,'injected title failure'); END",
    )?;
    let body = json!({"messages":[],"title":"after"});
    let err = shard.edit_conversation(&body).unwrap_err();
    assert!(format!("{err:#}").contains("injected title failure"));
    let (metadata, history) = stores.conversation_detail(id)?;
    assert_eq!(metadata.unwrap().title.as_deref(), Some("before"));
    assert_eq!(history.len(), 1);
    assert_eq!(history[0]["content"], "keep me");
    fault.execute_batch("DROP TRIGGER reject_title")?;
    drop(fault);
    let interrupted = stores.with_fault_hook(|phase| {
        anyhow::ensure!(
            format!("{phase:?}") != "EditMetadata",
            "interrupted after decision"
        );
        Ok(())
    });
    let e = interrupted
        .open_session(id)?
        .edit_conversation(&body)
        .unwrap_err();
    assert!(format!("{e:#}").contains("interrupted after decision"));
    drop((shard, interrupted, stores));
    let reopened = Stores::open(root.path())?;
    let (metadata, history) = reopened.conversation_detail(id)?;
    assert_eq!(metadata.unwrap().title.as_deref(), Some("after"));
    assert!(history.is_empty());
    Ok(())
}

#[test]
fn sync_reconciles_changed_user_display_and_preserves_unchanged_parts() -> Result<()> {
    let root = tempfile::tempdir()?;
    let stores = Stores::open(root.path())?;
    let shard = stores.create("typed", Default::default())?;
    let typed = json!([{"type":"text","text":"before"}]);
    shard.admit_user("typed-user", &typed, "typed")?;
    let mut rows = stores.sync_messages("typed")?;
    stores.sync_conversation("typed", None, &rows)?;
    assert_eq!(stores.conversation_detail("typed")?.1[0]["content"], typed);
    rows[0].content = "after".into();
    stores.sync_conversation("typed", None, &rows)?;
    stores.sync_conversation("typed", None, &rows)?;
    assert_eq!(
        stores.conversation_detail("typed")?.1[0]["content"],
        "after"
    );
    Ok(())
}

#[test]
fn synced_message_ids_can_be_restored_only_in_the_receiving_shard() -> Result<()> {
    let source = tempfile::tempdir()?;
    let destination = tempfile::tempdir()?;
    let a = Stores::open(source.path())?;
    a.create("shared", Default::default())?
        .write(|w| w.append("user", "from another device"))?;
    let b = Stores::open(destination.path())?;
    let target = b.create("shared", Default::default())?;
    let rows = a.sync_messages("shared")?;
    b.sync_conversation("shared", None, &rows)?;
    let snapshot = b.conversation_detail("shared")?.1;
    target.edit_conversation(&json!({"messages":[]}))?;
    target.edit_conversation(&json!({"messages":snapshot}))?;
    assert_eq!(b.sync_messages("shared")?[0].id, rows[0].id);
    let other = b.create("different", Default::default())?;
    let unqualified = json!({"messages":[{"id":rows[0].id,"role":"user","content":"foreign"}]});
    assert!(other.edit_conversation(&unqualified).is_err());
    Ok(())
}

#[test]
fn pin_completes_while_execution_is_held() -> Result<()> {
    let root = tempfile::tempdir()?;
    let stores = Stores::open(root.path())?;
    let shard = stores.create("running", Default::default())?;
    let lease = shard.acquire_execution()?;
    let (tx, rx) = std::sync::mpsc::channel();
    let writer =
        std::thread::spawn(move || tx.send(shard.edit_conversation(&json!({"pinned":true}))));
    let result = rx.recv_timeout(std::time::Duration::from_secs(2));
    drop(lease);
    writer.join().unwrap().ok();
    result??;
    assert!(stores.conversation_metadata("running")?.unwrap().pinned);
    Ok(())
}

#[test]
fn first_chat_retries_once_after_execution_lease_failure() -> Result<()> {
    let root = tempfile::tempdir()?;
    let stores = Stores::open(root.path())?;
    let id = uuid::Uuid::new_v4().to_string();
    let directory = root.path().join("state/locks/session-execution");
    if directory.is_dir() {
        std::fs::remove_dir(&directory)?;
    }
    std::fs::write(&directory, "unavailable")?;
    let content = json!("first turn");
    assert!(stores
        .admit_chat_execution(&id, "m", Some(&content), "typed")
        .is_err());
    let (row, messages) = stores.conversation_detail(&id)?;
    assert!(row.is_none() && messages.is_empty());
    std::fs::remove_file(directory)?;
    let (lease, sealed) = stores.admit_chat_execution(&id, "m", Some(&content), "typed")?;
    if !sealed {
        stores.admit_chat_message(&id, "m", Some(&content), "typed")?;
    }
    drop(lease);
    assert_eq!(stores.conversation_detail(&id)?.1.len(), 1);
    Ok(())
}

#[test]
fn combined_detail_matches_the_committed_edit() -> Result<()> {
    use std::sync::{Arc, Barrier};
    let root = tempfile::tempdir()?;
    let db = Stores::open(root.path())?;
    let shard = db.create("combined", Default::default())?;
    let body = |n| json!({"title":format!("version {n}"),
        "messages":[{"id":"message","role":"assistant","content":format!("version {n}")}]});
    shard.edit_conversation(&body(0))?;
    let barrier = Arc::new(Barrier::new(2));
    std::thread::scope(|scope| -> Result<()> {
        let start = barrier.clone();
        let writer = scope.spawn(move || -> Result<()> {
            start.wait();
            for n in 1..=60 { shard.edit_conversation(&body(n))?; }
            Ok(())
        });
        barrier.wait();
        loop {
            let (row, messages) = db.conversation_detail("combined")?;
            assert_eq!(row.unwrap().title.as_deref(), messages[0]["content"].as_str());
            if writer.is_finished() { break; }
        }
        writer.join().unwrap()
    })
}

#[test]
fn imported_owner_accepts_all_uuid_spellings_on_restore() -> Result<()> {
    let source = tempfile::tempdir()?;
    let a = Stores::open(source.path())?;
    a.create("shared", Default::default())?
        .write(|w| w.append("user", "received"))?;
    let mut rows = a.sync_messages("shared")?;
    let uuid = uuid::Uuid::parse_str(&rows[0].id)?;
    for prefix in ["", "notice-"] {
        for spelling in [
            uuid.to_string().to_uppercase(),
            uuid.simple().to_string(),
            uuid.braced().to_string(),
            uuid.urn().to_string(),
        ] {
            let destination = tempfile::tempdir()?;
            let b = Stores::open(destination.path())?;
            let shard = b.create("shared", Default::default())?;
            rows[0].id = format!("{prefix}{spelling}");
            b.sync_conversation("shared", None, &rows)?;
            let mut snapshot = b.conversation_detail("shared")?.1;
            shard.edit_conversation(&json!({"messages":[]}))?;
            snapshot[0]["id"] = json!(format!("{prefix}{uuid}"));
            shard.edit_conversation(&json!({"messages":snapshot}))?;
            assert_eq!(b.conversation_detail("shared")?.1[0]["content"], "received");
        }
    }
    Ok(())
}
