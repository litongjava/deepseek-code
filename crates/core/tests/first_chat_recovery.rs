use anyhow::{ensure, Result};
use deepseek_code_core::db::session_store::{Phase, Stores};
use serde_json::json;

#[test]
fn first_chat_recovery() -> Result<()> {
    let root = tempfile::tempdir()?;
    let stores = Stores::open(root.path())?;
    let fault = stores.with_fault_hook(|p| {
        if p == Phase::BeforePublishCommit {
            return Err(std::io::Error::other("simulated recoverable publish I/O failure").into());
        }
        Ok(())
    });
    let content = json!("first turn");
    let result = fault.admit_chat_execution("publication", "model", Some(&content), "typed");
    let error = result.err().expect("first admission should fail");
    println!("First admission returned: {error:#}");
    assert_eq!(stores.conversation_detail("publication")?.1.len(), 1);
    let (lease, sealed) =
        stores.admit_chat_execution("publication", "model", Some(&content), "typed")?;
    if !sealed {
        stores.admit_chat_message("publication", "model", Some(&content), "typed")?;
    }
    drop(lease);
    let (_, messages) = stores.conversation_detail("publication")?;
    let texts: Vec<_> = messages.iter().map(|m| &m["content"]).collect();
    println!("Retry sealed_first={sealed}, messages={texts:?}");
    ensure!(messages.len() == 1 && sealed);
    let (_lease, sealed_again) =
        stores.admit_chat_execution("publication", "model", Some(&content), "typed")?;
    assert!(!sealed_again);
    Ok(())
}

const RETRY: &str = "00000000-0000-4000-8000-000000000046";

fn submit(stores: &Stores, prompt: &str) -> Result<bool> {
    let content = json!(prompt);
    let (_lease, sealed) = stores.admit_chat_execution(RETRY, "model", Some(&content), "typed")?;
    if !sealed {
        stores.admit_chat_message(RETRY, "model", Some(&content), "typed")?;
    }
    Ok(sealed)
}

fn recover_first(stores: &Stores) -> Result<()> {
    let fault = stores.with_fault_hook(|phase| {
        ensure!(
            phase != Phase::BeforePublishCommit,
            "recoverable publication failure"
        );
        Ok(())
    });
    assert!(submit(&fault, "A").is_err());
    assert_eq!(texts(stores)?, [json!("A")]);
    Ok(())
}

fn texts(stores: &Stores) -> Result<Vec<serde_json::Value>> {
    Ok(stores
        .conversation_detail(RETRY)?
        .1
        .into_iter()
        .map(|m| m["content"].clone())
        .collect())
}

#[test]
fn rewind_then_resubmit_restores_one_prompt() -> Result<()> {
    use deepseek_code_core::db::session_store::Rewind;
    let root = tempfile::tempdir()?;
    let stores = Stores::open(root.path())?;
    recover_first(&stores)?;
    stores.rewind_messages(RETRY, Rewind::Tail(1))?;
    let sealed = submit(&stores, "A")?;
    assert_eq!(texts(&stores)?, [json!("A")]);
    assert!(!sealed);
    Ok(())
}

#[test]
fn intervening_submission_preserves_pending_retry() -> Result<()> {
    let root = tempfile::tempdir()?;
    let stores = Stores::open(root.path())?;
    recover_first(&stores)?;
    assert!(!submit(&stores, "B")?);
    let sealed = submit(&stores, "A")?;
    assert_eq!(texts(&stores)?, [json!("A"), json!("B")]);
    assert!(sealed);
    assert!(!submit(&stores, "A")?);
    assert_eq!(texts(&stores)?, [json!("A"), json!("B"), json!("A")]);
    Ok(())
}

#[test]
fn replacing_the_sealed_message_with_identical_text_clears_its_receipt() -> Result<()> {
    let root = tempfile::tempdir()?;
    let stores = Stores::open(root.path())?;
    recover_first(&stores)?;
    stores.open_session(RETRY)?.edit_conversation(&json!({
        "messages": [{"id":"replacement", "role":"user", "content":"A"}]
    }))?;
    assert!(!submit(&stores, "A")?);
    assert_eq!(texts(&stores)?, [json!("A"), json!("A")]);
    Ok(())
}

#[test]
fn upgrade_discards_all_legacy_first_message_receipts() -> Result<()> {
    for (rewind, reordered) in [(false, true), (true, false), (false, false)] {
        let root = tempfile::tempdir()?;
        let stores = Stores::open(root.path())?;
        recover_first(&stores)?;
        if reordered {
            let mut sealed = stores.sync_messages(RETRY)?.remove(0);
            let mut other = sealed.clone();
            other.id = uuid::Uuid::new_v4().to_string();
            sealed.sort_order = 1;
            stores.sync_conversation(RETRY, None, &[sealed, other])?;
        }
        drop(stores);
        let shard = rusqlite::Connection::open(
            root.path()
                .join("data/sessions")
                .join(RETRY)
                .join("session.db"),
        )?;
        shard.execute_batch("INSERT INTO peripheral_settings SELECT 'first-chat-receipt.' || incarnation,digest FROM manifest,first_chat_receipts;
            DROP TRIGGER first_chat_text_edit; DROP TRIGGER first_chat_typed_insert;
            DROP TRIGGER first_chat_typed_edit; DROP TRIGGER first_chat_typed_delete;
            DROP TABLE first_chat_receipts; PRAGMA user_version=11;")?;
        if rewind {
            // V11 rewind kept the now-orphaned digest.
            shard.execute_batch(
                "DELETE FROM messages; UPDATE manifest SET message_count=0,content_revision=2",
            )?;
        }
        drop(shard);
        let stores = Stores::open(root.path())?;
        if reordered {
            stores.rewind_messages(
                RETRY,
                deepseek_code_core::db::session_store::Rewind::Tail(1),
            )?;
        }
        let claimed = submit(&stores, "A")?;
        assert_eq!(
            texts(&stores)?,
            vec![json!("A"); if rewind { 1 } else { 2 }]
        );
        assert!(!claimed);
    }
    Ok(())
}
