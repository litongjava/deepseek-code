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
    lookup: impl Fn(&Path, Instant) -> Result<GitDirs> + Send + Sync + 'static,
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
    install_lookup(&owner, move |cwd, deadline| {
        let dirs = GitDirs::lookup(cwd, deadline)?;
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
    install_lookup(&owner, move |cwd, deadline| {
        let dirs = GitDirs::lookup(cwd, deadline)?;
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

fn queue_nine_managed_senders(
    ctx: &ServiceContext,
    store: &SessionStore,
    first: &str,
    recipient: &str,
) -> Vec<(Message, PathBuf)> {
    (0..9)
        .map(|i| {
            let sender = if i == 0 {
                first.to_owned()
            } else {
                store.fork(recipient).unwrap()
            };
            let path = bind_managed_worktree(ctx, store, &sender);
            let message = send(ctx, &sender, recipient, &format!("sender {i}")).unwrap();
            (message, path)
        })
        .collect()
}

#[test]
fn a_cold_one_message_drain_resolves_only_the_first_of_nine_senders() {
    let (_temp, context, store, first, recipient) = fixture();
    let source = store
        .workspace(&recipient, &context.paths.data_dir.join("data/workspaces"))
        .unwrap();
    let messages = queue_nine_managed_senders(&context, &store, &first, &recipient);
    let owner = Consumer::attach(&context.paths.data_dir, &recipient)
        .unwrap()
        .unwrap();
    let lookups = Arc::new(Mutex::new(Vec::new()));
    let calls = lookups.clone();
    install_lookup(&owner, move |cwd, deadline| {
        calls.lock().unwrap().push(cwd.to_path_buf());
        std::thread::sleep(Duration::from_millis(300));
        GitDirs::lookup(cwd, deadline)
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
    assert_eq!(calls.as_slice(), &[messages[0].1.clone(), source]);
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
    install_lookup(&owner, move |cwd, deadline| {
        calls.lock().unwrap().push(cwd.to_path_buf());
        GitDirs::lookup(cwd, deadline)
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
    install_lookup(&owner, move |cwd, deadline| {
        if first.swap(false, Ordering::SeqCst) {
            return Err(GitLookupTimeout.into());
        }
        GitDirs::lookup(cwd, deadline)
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
    install_lookup(&recovered, |_, _| {
        panic!("receipt-only recovery started Git")
    });
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
    install_lookup(&owner, move |cwd, deadline| {
        let dirs = GitDirs::lookup(cwd, deadline)?;
        if cwd == from {
            fs::copy(from.join(".git"), to.join(".git"))?;
        }
        Ok(dirs)
    });
    assert!(owner.target.drain(1).unwrap().is_empty());
    assert_blocked(&context, &store, &recipient, &message);
}

#[test]
fn a_local_worktree_redirect_blocks_prepared_claims_and_new_enqueues() -> Result<()> {
    use crate::worktree::session::git;
    let (temp, ctx, store, sender, recipient) = fixture();
    let from = bind_managed_worktree(&ctx, &store, &sender);
    let to = bind_managed_worktree(&ctx, &store, &recipient);
    let other = temp.path().join("other");
    fs::create_dir(&other)?;
    git(&to, &["config", "extensions.worktreeConfig", "true"])?;
    git(
        &to,
        &[
            "config",
            "--worktree",
            "core.worktree",
            to.to_str().unwrap(),
        ],
    )?;
    let config =
        PathBuf::from(git(&to, &["rev-parse", "--absolute-git-dir"])?).join("config.worktree");
    let modified = fs::metadata(&config)?.modified()?;
    let message = send(&ctx, &sender, &recipient, "prepared recipient")?;
    let owner = Consumer::attach(&ctx.paths.data_dir, &recipient)?.unwrap();
    let changed = Arc::new(AtomicBool::new(false));
    let changed_in_lookup = changed.clone();
    install_lookup(&owner, move |cwd, deadline| {
        let dirs = GitDirs::lookup(cwd, deadline)?;
        if cwd == from {
            // Recipient preparation has finished. Preserve config metadata so
            // a timestamp-only writer check would still accept its snapshot.
            let before = git(
                &to,
                &["rev-parse", "--absolute-git-dir", "--git-common-dir"],
            )?;
            git(
                &to,
                &[
                    "config",
                    "--worktree",
                    "core.worktree",
                    other.to_str().unwrap(),
                ],
            )?;
            fs::File::options()
                .write(true)
                .open(&config)?
                .set_modified(modified)?;
            assert_eq!(
                git(
                    &to,
                    &["rev-parse", "--absolute-git-dir", "--git-common-dir"]
                )?,
                before
            );
            assert_eq!(
                PathBuf::from(git(&to, &["rev-parse", "--show-toplevel"])?).canonicalize()?,
                other.canonicalize()?
            );
            changed_in_lookup.store(true, Ordering::SeqCst);
        }
        Ok(dirs)
    });
    assert!(owner.target.drain(1)?.is_empty());
    assert!(changed.load(Ordering::SeqCst));
    assert_blocked(&ctx, &store, &recipient, &message);
    let generation: Option<i64> = ctx.db.fixture_directory(|c| {
        c.query_row(
            "SELECT generation FROM session_inbox WHERE id=?1",
            [&message.id],
            |r| r.get(0),
        )
    })??;
    assert_eq!(generation, None);
    // A fresh snapshot must reject Git's redirected top level as well.
    let err = send(&ctx, &sender, &recipient, "new enqueue").unwrap_err();
    assert!(
        err.to_string()
            .contains("Git worktree identity changed during lookup"),
        "{err:#}"
    );
    assert_eq!(inbox(&ctx, &recipient, false)?.len(), 1);
    Ok(())
}

#[test]
fn a_managed_project_subdirectory_is_bound_to_its_saved_checkout_root() -> Result<()> {
    use crate::worktree::session::{git, prepare};
    let (temp, ctx, store, sender, recipient) = fixture();
    let source = temp.path().join("project");
    git(temp.path(), &["init", "-q"])?;
    fs::write(source.join("tracked"), "")?;
    git(temp.path(), &["add", "project/tracked"])?;
    git(
        temp.path(),
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "-qm",
            "initial",
        ],
    )?;
    let worktree = prepare(&source, None, true)?;
    let project = crate::services::session_trust::inspect(&ctx.db, &sender)?;
    ctx.db.fixture_directory(|c| {
        worktree.save(c, &sender, &project.id, source.to_str().unwrap())
    })??;
    let owner = Consumer::attach(&ctx.paths.data_dir, &recipient)?.unwrap();
    send(&ctx, &sender, &recipient, "project subdirectory")?;
    let delivered = drain_pending(&owner.target)?;
    assert_eq!(delivered.len(), 1);
    assert!(delivered[0].contains("project subdirectory"));

    // The nested marker resolves to the very same Git and common directories,
    // but would make the project subdirectory its own checkout root.
    let marker = worktree.path.join(".git");
    fs::copy(worktree.path.parent().unwrap().join(".git"), &marker)?;
    assert_eq!(
        PathBuf::from(git(&worktree.path, &["rev-parse", "--show-toplevel"])?).canonicalize()?,
        worktree.path.canonicalize()?
    );
    let err = send(&ctx, &sender, &recipient, "nested marker").unwrap_err();
    assert!(
        err.to_string()
            .contains("Git worktree root does not match the saved session"),
        "{err:#}"
    );
    assert_eq!(inbox(&ctx, &recipient, false)?.len(), 1);
    assert_eq!(store.history(&recipient)?.len(), 1);
    Ok(())
}

fn run_environment_child(command: &mut std::process::Command) -> Result<()> {
    ensure!(command.status()?.success(), "Git environment child failed");
    Ok(())
}

// Only the child receives Git discovery variables. Other libtest threads can
// create and validate real repositories while these cases run.
fn check_discovery_environment(
    redirect: bool,
    run: impl FnOnce(&Path, &mut std::process::Command) -> Result<()>,
) -> Result<()> {
    use std::process::Command;
    if let Some(root) = std::env::var_os("I49_DISCOVERY_TEST_ROOT") {
        let ctx = context(Path::new(&root))?;
        let store = SessionStore::new(ctx.db.clone());
        let recipient = std::env::var("I49_DISCOVERY_TEST_RECIPIENT")?;
        let owner = Consumer::attach(&ctx.paths.data_dir, &recipient)?.unwrap();
        if redirect {
            assert!(owner.target.drain(1)?.is_empty());
            let row = &inbox(&ctx, &recipient, false)?[0];
            assert_eq!(row.state, "blocked");
            assert_eq!(
                row.blocked_reason.as_deref(),
                Some("Git directory override in effect: peer identity cannot be verified")
            );
            assert!(store.history(&recipient)?.is_empty());
            let (generation, consumers): (Option<i64>, i64) = ctx.db.fixture_directory(|c| {
                c.query_row(
                    "SELECT generation, (SELECT count(*) FROM session_inbox_consumers WHERE session_id=?1)
                     FROM session_inbox WHERE recipient=?1",
                    [&recipient],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
            })??;
            assert_eq!((generation, consumers), (None, 0));
            let sender = &row.sender;
            let err = send(&ctx, sender, &recipient, "another message").unwrap_err();
            assert!(err.is::<identity::GitDirectoryOverride>(), "{err:#}");
            assert_eq!(inbox(&ctx, &recipient, false)?.len(), 1);
        } else {
            let inputs = drain_pending(&owner.target)?;
            assert_eq!(inputs.len(), 1);
            assert!(inputs[0].contains("ordinary discovery"));
            assert_eq!(
                inbox(&ctx, &recipient, false)?[0].state,
                "context_committed"
            );
            assert!(owner.target.drain(1)?.is_empty());
            assert_eq!(store.history(&recipient)?.len(), 1);
        }
        return Ok(());
    }

    let (_temp, ctx, store, sender, recipient) = fixture();
    let tree = bind_managed_worktree(&ctx, &store, &sender);
    send(&ctx, &sender, &recipient, "ordinary discovery")?;
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args([
            "--exact",
            std::thread::current().name().unwrap(),
            "--nocapture",
        ])
        .env("I49_DISCOVERY_TEST_ROOT", &ctx.paths.data_dir)
        .env("I49_DISCOVERY_TEST_RECIPIENT", &recipient);
    run(&tree, &mut command)
}

#[test]
fn directory_redirects_block_managed_peers_before_any_claim() -> Result<()> {
    check_discovery_environment(true, |tree, cmd| {
        let git_dir = crate::worktree::session::git(tree, &["rev-parse", "--absolute-git-dir"])?;
        let common = crate::worktree::session::common_dir(tree)?;
        for (key, value) in [
            ("GIT_DIR", Path::new(&git_dir)),
            ("GIT_COMMON_DIR", &common),
            ("GIT_WORK_TREE", tree),
        ] {
            run_environment_child(cmd.env(key, value))?;
            cmd.env_remove(key);
        }
        Ok(())
    })
}

#[test]
fn cross_filesystem_discovery_still_delivers_once() -> Result<()> {
    check_discovery_environment(false, |_, cmd| {
        run_environment_child(cmd.env("GIT_DISCOVERY_ACROSS_FILESYSTEM", "1"))
    })
}

#[test]
fn runtime_config_count_blocks_enqueue_and_claim() -> Result<()> {
    check_discovery_environment(true, |tree, cmd| {
        run_environment_child(
            cmd.env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "core.worktree")
                .env("GIT_CONFIG_VALUE_0", tree.parent().unwrap()),
        )
    })
}

#[test]
fn runtime_config_parameters_block_enqueue_and_claim() -> Result<()> {
    check_discovery_environment(true, |_, cmd| {
        run_environment_child(cmd.env("GIT_CONFIG_PARAMETERS", "'core.worktree'='..'"))
    })
}

#[test]
fn unlisted_git_variables_block_enqueue_and_claim() -> Result<()> {
    check_discovery_environment(true, |_, cmd| {
        for key in [
            "GIT_NAMESPACE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
            "GIT_CONFIG_GLOBAL",
            "GIT_CONFIG_SYSTEM",
            "GIT_INDEX_FILE",
            "GIT_UNKNOWN_FUTURE_CONTROL",
        ] {
            run_environment_child(cmd.env(key, "value"))?;
            cmd.env_remove(key);
        }
        Ok(())
    })
}

#[test]
fn session_git_trace_allows_delivery_without_tracing_the_identity_child() -> Result<()> {
    check_discovery_environment(false, |_, cmd| {
        let dir = tempfile::tempdir()?;
        let trace = dir.path().join("git-trace");
        run_environment_child(cmd.env("GIT_TRACE", &trace))?;
        assert!(!trace.exists());
        Ok(())
    })
}

#[test]
fn one_preparation_budget_bounds_eight_candidates_and_retries_the_rest_once() -> Result<()> {
    use std::collections::HashSet;
    let (_temp, ctx, store, first, recipient) = fixture();
    let messages = queue_nine_managed_senders(&ctx, &store, &first, &recipient);
    let owner = Consumer::attach(&ctx.paths.data_dir, &recipient)?.unwrap();
    let clipped = Arc::new(AtomicBool::new(false));
    let saw_short_deadline = clipped.clone();
    install_lookup(&owner, move |cwd, end| {
        let delay = Duration::from_millis(700);
        let remaining = end.saturating_duration_since(Instant::now());
        if remaining < delay {
            saw_short_deadline.store(true, Ordering::Relaxed);
        }
        std::thread::sleep(delay.min(remaining));
        // Use the production runner with the original deadline, including the
        // part already spent waiting on simulated Git latency.
        GitDirs::lookup(cwd, end)
    });

    let mut delivered = HashSet::new();
    for poll in 1..=10 {
        let start = Instant::now();
        let inputs = owner
            .delivery
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .poll_turn(true, 8)?;
        let wall = start.elapsed();
        eprintln!(
            "engine poll {poll}: elapsed={wall:?} delivered={}",
            inputs.len()
        );
        if poll == 1 {
            assert!(wall < Duration::from_millis(3500), "{wall:?}");
            assert!((1..8).contains(&inputs.len()));
            assert!(
                clipped.load(Ordering::Relaxed),
                "no lookup received the partial remaining budget"
            );
        }
        for input in inputs {
            assert!(
                delivered.insert(input.message_id.unwrap()),
                "duplicate peer delivery"
            );
        }
        let (rows, owner_generation) = ctx.db.fixture_directory(|c| -> rusqlite::Result<_> {
            let mut query = c.prepare(
                "SELECT id,state,generation,blocked_reason FROM session_inbox WHERE recipient=?1 ORDER BY rowid",
            )?;
            let rows = query.query_map([&recipient], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?,
                    r.get::<_, Option<i64>>(2)?, r.get::<_, Option<String>>(3)?))
            })?.collect::<rusqlite::Result<Vec<_>>>()?;
            let generation: i64 = c.query_row(
                "SELECT generation FROM session_inbox_consumers WHERE session_id=?1",
                [&recipient], |r| r.get(0),
            )?;
            Ok((rows, generation))
        })??;
        assert_eq!(rows.len(), messages.len());
        assert_eq!(owner_generation as usize, delivered.len());
        for (i, (id, state, generation, reason)) in rows.into_iter().enumerate() {
            assert_eq!(id, messages[i].0.id);
            assert!(reason.is_none());
            if delivered.contains(&id) {
                assert_eq!(state, "context_committed");
                assert_eq!(generation, Some(i as i64 + 1));
            } else {
                assert_eq!(state, "queued");
                assert_eq!(generation, None);
            }
        }
        assert_eq!(store.history(&recipient)?.len(), delivered.len());
        if delivered.len() == messages.len() {
            break;
        }
    }
    assert_eq!(delivered.len(), 9);
    assert!(owner.target.drain(8)?.is_empty());
    assert_eq!(store.history(&recipient)?.len(), 9);
    Ok(())
}
