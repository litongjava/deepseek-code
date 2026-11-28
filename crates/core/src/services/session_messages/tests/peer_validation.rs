use super::*;
use crate::services::session_messages::identity::{GitDirs, GitLookupTimeout, Resolver};
use std::{
    fs,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
    time::{Duration, Instant, UNIX_EPOCH},
};

fn install_lookup(
    owner: &Consumer,
    lookup: impl Fn(&Path) -> Result<GitDirs> + Send + Sync + 'static,
) {
    owner
        .delivery
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .set_peer_resolver(Resolver::new(lookup));
}

fn assert_blocked(
    context: &ServiceContext,
    store: &SessionStore,
    recipient: &str,
    message: &Message,
) {
    let rows = inbox(context, recipient, false).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, message.id);
    assert_eq!(rows[0].state, "blocked");
    assert!(store.history(recipient).unwrap().is_empty());
}

#[cfg(unix)]
#[test]
fn trusted_folder_retargeted_during_preparation_blocks_the_claim() {
    let (temp, context, store, sender, recipient) = fixture();
    bind_managed_worktree(&context, &store, &sender);
    let folder = temp.path().join("secondary");
    let moved = temp.path().join("moved");
    fs::create_dir(&folder).unwrap();
    context
        .db
        .fixture_directory(|c| {
            c.execute(
                "INSERT INTO project_folders(project_id,path,trusted,sort_order)
            SELECT project_id,?2,1,1 FROM conversations WHERE id=?1",
                rusqlite::params![sender, folder.canonicalize().unwrap().to_str().unwrap()],
            )
        })
        .unwrap()
        .unwrap();
    let message = send(&context, &sender, &recipient, "stale folder").unwrap();
    let owner = Consumer::attach(&context.paths.data_dir, &recipient)
        .unwrap()
        .unwrap();
    let changed = AtomicBool::new(false);
    install_lookup(&owner, move |cwd| {
        let dirs = GitDirs::lookup(cwd)?;
        // Both endpoints passed folder canonicalization before the sender lookup.
        if !changed.swap(true, Ordering::SeqCst) {
            fs::rename(&folder, &moved)?;
            std::os::unix::fs::symlink(&moved, &folder)?;
        }
        Ok(dirs)
    });
    assert!(owner.target.drain(1).unwrap().is_empty());
    assert_blocked(&context, &store, &recipient, &message);
}

#[test]
fn hook_directory_recreated_after_recipient_preparation_blocks_the_claim() {
    let (temp, context, store, sender, recipient) = fixture();
    bind_managed_worktree(&context, &store, &sender);
    let hook = temp.path().join("hook-tree");
    fs::create_dir(&hook).unwrap();
    let created = fs::metadata(&hook)
        .unwrap()
        .created()
        .unwrap()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64;
    context.db.fixture_directory(|c| -> Result<()> {
        c.execute("INSERT INTO conversation_worktrees(conversation_id,project_id,source_path,repo_path,path,branch)
            SELECT id,project_id,workspace_path,workspace_path,?2,'hook' FROM conversations WHERE id=?1",
            rusqlite::params![recipient,hook.to_str().unwrap()])?;
        c.execute("INSERT INTO conversation_worktree_backends VALUES(?1,'hook',?2)",
            rusqlite::params![recipient,serde_json::json!({"canonical_path":hook.canonicalize()?,"created_ns":created}).to_string()])?;
        Ok(())
    }).unwrap().unwrap();
    let message = send(&context, &sender, &recipient, "stale hook").unwrap();
    let owner = Consumer::attach(&context.paths.data_dir, &recipient)
        .unwrap()
        .unwrap();
    let changed = AtomicBool::new(false);
    install_lookup(&owner, move |cwd| {
        let dirs = GitDirs::lookup(cwd)?;
        if !changed.swap(true, Ordering::SeqCst) {
            fs::rename(&hook, hook.with_extension("old"))?;
            fs::create_dir(&hook)?;
            assert_ne!(
                fs::metadata(&hook)?
                    .created()?
                    .duration_since(UNIX_EPOCH)?
                    .as_nanos(),
                u128::from(created)
            );
        }
        Ok(dirs)
    });
    assert!(owner.target.drain(1).unwrap().is_empty());
    assert_blocked(&context, &store, &recipient, &message);
}

#[test]
fn a_cold_one_message_drain_resolves_only_the_first_of_nine_senders() {
    let (_temp, context, store, first, recipient) = fixture();
    let source = store
        .workspace(&recipient, &context.paths.data_dir.join("data/workspaces"))
        .unwrap();
    let mut paths = Vec::new();
    for index in 0..9 {
        let sender = if index == 0 {
            first.clone()
        } else {
            store.fork(&recipient).unwrap()
        };
        paths.push(bind_managed_worktree(&context, &store, &sender));
        send(&context, &sender, &recipient, &format!("sender {index}")).unwrap();
    }
    let owner = Consumer::attach(&context.paths.data_dir, &recipient)
        .unwrap()
        .unwrap();
    let lookups = Arc::new(Mutex::new(Vec::new()));
    let calls = lookups.clone();
    install_lookup(&owner, move |cwd| {
        calls.lock().unwrap().push(cwd.to_path_buf());
        std::thread::sleep(Duration::from_millis(300));
        GitDirs::lookup(cwd)
    });
    let started = Instant::now();
    let inputs = owner.target.drain(1).unwrap();
    let elapsed = started.elapsed();
    let calls = lookups.lock().unwrap();
    eprintln!(
        "cold one-message drain: {} Git lookups, {elapsed:?}",
        calls.len()
    );
    assert_eq!(inputs.len(), 1);
    assert!(inputs[0].contains("sender 0"));
    assert_eq!(calls.as_slice(), &[paths[0].clone(), source]);
    let rows = inbox(&context, &recipient, false).unwrap();
    assert_eq!(rows.iter().filter(|m| m.state == "queued").count(), 8);
    assert_eq!(store.history(&recipient).unwrap().len(), 1);
}

#[test]
fn the_remaining_byte_budget_skips_the_next_sender_lookup() {
    let (_temp, context, store, first, recipient) = fixture();
    let second = store.fork(&recipient).unwrap();
    let path = bind_managed_worktree(&context, &store, &second);
    for sender in [&first, &second] {
        send(&context, sender, &recipient, &"a".repeat(65_536)).unwrap();
    }
    let owner = Consumer::attach(&context.paths.data_dir, &recipient)
        .unwrap()
        .unwrap();
    let lookups = Arc::new(Mutex::new(Vec::new()));
    let calls = lookups.clone();
    install_lookup(&owner, move |cwd| {
        calls.lock().unwrap().push(cwd.to_path_buf());
        GitDirs::lookup(cwd)
    });
    assert_eq!(owner.target.drain(8).unwrap().len(), 1);
    assert!(lookups.lock().unwrap().is_empty());
    assert_eq!(owner.target.drain(8).unwrap().len(), 1);
    assert!(lookups.lock().unwrap().contains(&path));
    assert!(owner.target.drain(8).unwrap().is_empty());
}

#[test]
fn a_lookup_timeout_defers_without_blocking_or_claiming_the_message() {
    let (_temp, context, store, sender, recipient) = fixture();
    bind_managed_worktree(&context, &store, &sender);
    let message = send(&context, &sender, &recipient, "retry after timeout").unwrap();
    let owner = Consumer::attach(&context.paths.data_dir, &recipient)
        .unwrap()
        .unwrap();
    let first = AtomicBool::new(true);
    install_lookup(&owner, move |cwd| {
        if first.swap(false, Ordering::SeqCst) {
            return Err(GitLookupTimeout.into());
        }
        GitDirs::lookup(cwd)
    });
    assert!(owner.target.drain(1).unwrap().is_empty());
    let row = context
        .db
        .fixture_directory(|c| {
            c.query_row(
                "SELECT state,generation,blocked_reason FROM session_inbox WHERE id=?1",
                [&message.id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<i64>>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                },
            )
        })
        .unwrap()
        .unwrap();
    assert_eq!(row, ("queued".into(), None, None));
    assert!(store.history(&recipient).unwrap().is_empty());
    assert_eq!(drain_pending(&owner.target).unwrap().len(), 1);
    assert_eq!(
        inbox(&context, &recipient, false).unwrap()[0].state,
        "context_committed"
    );
    assert_eq!(store.history(&recipient).unwrap().len(), 1);
    assert!(owner.target.drain(1).unwrap().is_empty());
}

#[test]
fn a_cold_receipt_only_recovery_acks_without_resolving_either_endpoint() {
    let (_temp, context, store, sender, recipient) = fixture();
    bind_managed_worktree(&context, &store, &sender);
    bind_managed_worktree(&context, &store, &recipient);
    send(&context, &sender, &recipient, "already durable").unwrap();
    let attach = || {
        Consumer::attach(&context.paths.data_dir, &recipient)
            .unwrap()
            .unwrap()
    };
    let owner = attach();
    context
        .db
        .fixture_directory(|c| {
            c.execute_batch(
                "CREATE TRIGGER fail_peer_ack BEFORE UPDATE OF state ON session_inbox
        WHEN NEW.state='context_committed' BEGIN SELECT RAISE(ABORT,'stop after receipt'); END;",
            )
        })
        .unwrap()
        .unwrap();
    let error = drain_pending(&owner.target).unwrap_err();
    assert_eq!(crate::db::sqlite_extended_code(&error), Some(1811));
    assert_eq!(store.history(&recipient).unwrap().len(), 1);
    drop(owner);
    context
        .db
        .fixture_directory(|c| c.execute_batch("DROP TRIGGER fail_peer_ack"))
        .unwrap()
        .unwrap();
    let recovered = attach();
    install_lookup(&recovered, |_| panic!("receipt-only recovery started Git"));
    assert!(recovered.target.drain(1).unwrap().is_empty());
    assert_eq!(
        inbox(&context, &recipient, false).unwrap()[0].state,
        "context_committed"
    );
    assert_eq!(store.history(&recipient).unwrap().len(), 1);
}

#[test]
fn a_bounded_poll_revisits_blocked_rows_without_starving_a_recovered_row() {
    let (_temp, context, store, sender, recipient) = fixture();
    let first = send(&context, &sender, &recipient, "unavailable").unwrap();
    let second = send(&context, &sender, &recipient, "recovered").unwrap();
    context.db.fixture_directory(|c| c.execute(
        "UPDATE session_inbox SET state='blocked', project_source=CASE WHEN id=?1 THEN 'retired' ELSE project_source END",
        [&first.id],
    )).unwrap().unwrap();
    let owner = Consumer::attach(&context.paths.data_dir, &recipient)
        .unwrap()
        .unwrap();
    assert!(owner.target.drain(1).unwrap().is_empty());
    let delivered = owner.target.drain(1).unwrap();
    assert_eq!(delivered.len(), 1);
    assert!(delivered[0].contains("recovered"));
    let rows = inbox(&context, &recipient, false).unwrap();
    assert_eq!(
        rows.iter().find(|m| m.id == first.id).unwrap().state,
        "blocked"
    );
    assert_eq!(
        rows.iter().find(|m| m.id == second.id).unwrap().state,
        "context_committed"
    );
    assert_eq!(store.history(&recipient).unwrap().len(), 1);
}

#[test]
fn retargeting_the_recipient_gitfile_within_one_repository_blocks_the_claim() {
    let (_temp, context, store, sender, recipient) = fixture();
    let from = bind_managed_worktree(&context, &store, &sender);
    let to = bind_managed_worktree(&context, &store, &recipient);
    let message = send(&context, &sender, &recipient, "changed gitdir").unwrap();
    let owner = Consumer::attach(&context.paths.data_dir, &recipient)
        .unwrap()
        .unwrap();
    install_lookup(&owner, move |cwd| {
        let dirs = GitDirs::lookup(cwd)?;
        if cwd == from {
            fs::copy(from.join(".git"), to.join(".git"))?;
        }
        Ok(dirs)
    });
    assert!(owner.target.drain(1).unwrap().is_empty());
    assert_blocked(&context, &store, &recipient, &message);
}
