use deepseek_code_core::db::{message_repo, write_tx};
use deepseek_code_core::runtime::{RuntimePaths, SessionStore, DATABASE_FILE};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Duration;

#[test]
fn parallel_cli_sessions_and_messages_do_not_upgrade_stale_wal_snapshots() {
    let temp = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
    let db = paths.open_database().unwrap();
    let barrier = Arc::new(Barrier::new(6));
    let threads = (0..6)
        .map(|_| {
            let db = db.clone();
            let workspace = temp.path().to_path_buf();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let store = SessionStore::new(db);
                barrier.wait();
                for _ in 0..10 {
                    let id = store.create(&workspace, "fixture", "fixture").unwrap();
                    store.append(&id, "user", "hello").unwrap();
                }
            })
        })
        .collect::<Vec<_>>();
    for thread in threads {
        thread.join().unwrap();
    }
}

#[test]
fn separate_hosts_can_open_and_initialize_the_same_database() {
    let temp = tempfile::tempdir().unwrap();
    let barrier = Arc::new(Barrier::new(6));
    let threads = (0..6)
        .map(|_| {
            let data = temp.path().join("data");
            let workspace = temp.path().to_path_buf();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..3 {
                    let db = RuntimePaths::new(data.clone())
                        .unwrap()
                        .open_database()
                        .unwrap();
                    let store = SessionStore::new(db);
                    let id = store.create(&workspace, "fixture", "fixture").unwrap();
                    store.append(&id, "user", "hello").unwrap();
                }
            })
        })
        .collect::<Vec<_>>();
    for thread in threads {
        thread.join().unwrap();
    }
}

fn turn_fixture() -> (
    tempfile::TempDir,
    Arc<deepseek_code_core::db::DbManager>,
    String,
    std::path::PathBuf,
) {
    let temp = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
    let db = paths.open_database().unwrap();
    let store = SessionStore::new(db.clone());
    let session = store.create(temp.path(), "fixture", "fixture").unwrap();
    let message = store.append(&session, "assistant", "").unwrap();
    let path = temp.path().join("data").join(DATABASE_FILE);
    (temp, db, message, path)
}

/// Hold the write lock until the returned sender fires, so the test controls
/// exactly when the competing writer releases it.
fn hold_write_lock(
    path: std::path::PathBuf,
) -> (std::thread::JoinHandle<()>, std::sync::mpsc::Sender<()>) {
    let writer = hold_write_lock_guarded(path, Duration::from_secs(30));
    (writer.holder, writer.release)
}

/// A held writer that reports when the lock was taken and when it was released,
/// so a test can assert ordering without depending on wall-clock margins.
///
/// `guard` bounds the *fixture* wait only; it must be longer than the production
/// retry budget when a test needs the writer to outlive that budget.
struct HeldWriter {
    holder: std::thread::JoinHandle<()>,
    release: std::sync::mpsc::Sender<()>,
    held: Arc<AtomicBool>,
    released: Arc<AtomicBool>,
}

fn hold_write_lock_guarded(path: std::path::PathBuf, guard: Duration) -> HeldWriter {
    let (locked_tx, locked_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let held = Arc::new(AtomicBool::new(false));
    let released = Arc::new(AtomicBool::new(false));
    let holder_held = held.clone();
    let holder_released = released.clone();
    let holder = std::thread::spawn(move || {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        holder_held.store(true, Ordering::SeqCst);
        locked_tx.send(()).unwrap();
        release_rx.recv_timeout(guard).unwrap();
        conn.execute_batch("ROLLBACK").unwrap();
        holder_released.store(true, Ordering::SeqCst);
    });
    locked_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    HeldWriter {
        holder,
        release: release_tx,
        held,
        released,
    }
}

fn insert_tool_rows(
    tx: &rusqlite::Transaction<'_>,
    message: &str,
    rows: i64,
) -> anyhow::Result<()> {
    for index in 0..rows {
        message_repo::insert_tool_call(
            tx,
            &format!("{message}:{index}"),
            message,
            "Read",
            None,
            Some("output"),
            false,
            index,
        )?;
    }
    Ok(())
}

/// Longer than the pool's 5 s `busy_timeout`, so a single `BEGIN IMMEDIATE`
/// cannot survive it and only a retry at the whole-transaction boundary can.
///
/// The parent full-suite failure is consistent with a writer unavailable for at
/// least one busy timeout; this control reproduces that boundary deterministically,
/// not the specific historical timeline (which the preserved log cannot show).
const BEYOND_ONE_BUSY_TIMEOUT: Duration = Duration::from_millis(5_500);

/// The production retry budget for session writes. The fixture's own guard must
/// outlast it so the writer cannot release at the same instant the budget ends.
const SESSION_WRITE_DEADLINE: Duration = Duration::from_secs(30);
const DEADLINE_FIXTURE_GUARD: Duration = Duration::from_secs(45);

fn session_fixture() -> (
    tempfile::TempDir,
    Arc<deepseek_code_core::db::DbManager>,
    SessionStore,
    std::path::PathBuf,
) {
    let temp = tempfile::tempdir().unwrap();
    let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
    let db = paths.open_database().unwrap();
    let store = SessionStore::new(db.clone());
    let path = temp.path().join("data").join(DATABASE_FILE);
    (temp, db, store, path)
}

fn count(db: &deepseek_code_core::db::DbManager, sql: &str, id: &str) -> i64 {
    db.with_conn(|conn| conn.query_row(sql, [id], |row| row.get(0)))
        .unwrap()
        .unwrap()
}

fn scalar(db: &deepseek_code_core::db::DbManager, sql: &str) -> i64 {
    db.with_conn(|conn| conn.query_row(sql, [], |row| row.get(0)))
        .unwrap()
        .unwrap()
}

/// A session create that meets a writer held past one busy timeout must not end
/// the operation: it retries the whole transaction and then creates exactly one
/// session, one project and one folder.
#[test]
fn session_create_survives_a_writer_held_past_one_busy_timeout() {
    let (temp, db, store, path) = session_fixture();
    let writer = hold_write_lock_guarded(path, Duration::from_secs(30));
    let release = writer.release.clone();
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(BEYOND_ONE_BUSY_TIMEOUT);
        release.send(()).unwrap();
    });
    assert!(
        writer.held.load(Ordering::SeqCst),
        "the fixture writer was not held before the create"
    );
    let id = store.create(temp.path(), "fixture", "fixture").unwrap();
    // The holder stores `released` after ROLLBACK, so joining it first avoids a
    // scheduler false failure; the create can legitimately return in between.
    releaser.join().unwrap();
    writer.holder.join().unwrap();
    assert!(
        writer.released.load(Ordering::SeqCst),
        "the fixture writer never recorded its release"
    );
    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM conversations WHERE id=?1", &id),
        1
    );
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM project_folders WHERE project_id=(SELECT project_id FROM conversations WHERE id=?1)",
            &id
        ),
        1,
        "a retried create must not duplicate the project folder"
    );
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM projects WHERE id=(SELECT project_id FROM conversations WHERE id=?1)",
            &id
        ),
        1
    );
}

/// The same boundary for a message append: after the writer is released the
/// message exists exactly once, with the first sort order and a count of one.
#[test]
fn session_append_survives_a_writer_held_past_one_busy_timeout() {
    let (temp, db, store, path) = session_fixture();
    let id = store.create(temp.path(), "fixture", "fixture").unwrap();
    let writer = hold_write_lock_guarded(path, Duration::from_secs(30));
    let release = writer.release.clone();
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(BEYOND_ONE_BUSY_TIMEOUT);
        release.send(()).unwrap();
    });
    assert!(
        writer.held.load(Ordering::SeqCst),
        "the fixture writer was not held before the append"
    );
    let message = store.append(&id, "user", "after contention").unwrap();
    releaser.join().unwrap();
    writer.holder.join().unwrap();
    assert!(
        writer.released.load(Ordering::SeqCst),
        "the fixture writer never recorded its release"
    );
    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM messages WHERE id=?1", &message),
        1,
        "a retried append must not duplicate the message"
    );
    assert_eq!(
        count(
            &db,
            "SELECT message_count FROM conversations WHERE id=?1",
            &id
        ),
        1
    );
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM messages WHERE conversation_id=?1",
            &id
        ),
        1
    );
}

/// The retry budget itself must stop: a writer held past the whole deadline
/// surfaces a typed, named contention error with no partial rows, and the same
/// pooled database remains usable afterwards.
#[test]
fn session_create_gives_up_at_the_deadline_and_the_pool_stays_usable() {
    let (temp, db, store, path) = session_fixture();
    let writer = hold_write_lock_guarded(path, DEADLINE_FIXTURE_GUARD);
    let started = std::time::Instant::now();
    let error = store.create(temp.path(), "fixture", "fixture").unwrap_err();
    let elapsed = started.elapsed();
    assert!(deepseek_code_core::db::is_contention(&error), "{error:#}");
    let text = format!("{error:#}");
    assert!(text.contains("session create"), "{text}");
    assert!(
        elapsed >= SESSION_WRITE_DEADLINE,
        "the create gave up before its retry budget: {elapsed:?}"
    );
    assert!(!writer.released.load(Ordering::SeqCst));
    // Nothing from the abandoned attempts may survive.
    assert_eq!(scalar(&db, "SELECT COUNT(*) FROM conversations"), 0);
    assert_eq!(scalar(&db, "SELECT COUNT(*) FROM projects"), 0);
    assert_eq!(scalar(&db, "SELECT COUNT(*) FROM project_folders"), 0);
    writer.release.send(()).unwrap();
    writer.holder.join().unwrap();
    let id = store.create(temp.path(), "fixture", "fixture").unwrap();
    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM conversations WHERE id=?1", &id),
        1
    );
    assert_eq!(scalar(&db, "SELECT COUNT(*) FROM projects"), 1);
}

/// The same deadline contract for append: a typed named error, no message or
/// counter left behind, and a usable pool once the writer is released.
#[test]
fn session_append_gives_up_at_the_deadline_and_the_pool_stays_usable() {
    let (temp, db, store, path) = session_fixture();
    let id = store.create(temp.path(), "fixture", "fixture").unwrap();
    let writer = hold_write_lock_guarded(path, DEADLINE_FIXTURE_GUARD);
    let started = std::time::Instant::now();
    let error = store.append(&id, "user", "must not persist").unwrap_err();
    let elapsed = started.elapsed();
    assert!(deepseek_code_core::db::is_contention(&error), "{error:#}");
    let text = format!("{error:#}");
    assert!(text.contains("session append"), "{text}");
    assert!(
        elapsed >= SESSION_WRITE_DEADLINE,
        "the append gave up before its retry budget: {elapsed:?}"
    );
    assert!(!writer.released.load(Ordering::SeqCst));
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM messages WHERE conversation_id=?1",
            &id
        ),
        0
    );
    assert_eq!(
        count(
            &db,
            "SELECT message_count FROM conversations WHERE id=?1",
            &id
        ),
        0
    );
    writer.release.send(()).unwrap();
    writer.holder.join().unwrap();
    store.append(&id, "user", "persisted once").unwrap();
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM messages WHERE conversation_id=?1",
            &id
        ),
        1
    );
    assert_eq!(
        count(
            &db,
            "SELECT message_count FROM conversations WHERE id=?1",
            &id
        ),
        1
    );
}

/// A non-contention failure after the project rows were written must roll the
/// whole create back: no conversation, project, folder or trust row survives.
#[test]
fn a_failed_create_leaves_no_partial_project_or_session() {
    let (temp, db, store, _path) = session_fixture();
    db.with_conn(|conn| {
        conn.execute_batch(
            "CREATE TRIGGER reject_conversation BEFORE INSERT ON conversations \
             BEGIN SELECT RAISE(ABORT,'fixture create failure'); END;",
        )
    })
    .unwrap()
    .unwrap();
    let started = std::time::Instant::now();
    let error = store.create(temp.path(), "fixture", "fixture").unwrap_err();
    let elapsed = started.elapsed();
    assert!(!deepseek_code_core::db::is_contention(&error), "{error:#}");
    assert!(
        format!("{error:#}").contains("fixture create failure"),
        "{error:#}"
    );
    assert!(
        elapsed < Duration::from_secs(1),
        "a non-contention error must surface immediately, not after {elapsed:?}"
    );
    for (sql, table) in [
        (
            "SELECT COUNT(*) FROM conversations WHERE 1=1",
            "conversation",
        ),
        ("SELECT COUNT(*) FROM projects WHERE 1=1", "project"),
        (
            "SELECT COUNT(*) FROM project_folders WHERE 1=1",
            "project folder",
        ),
    ] {
        let remaining: i64 = db
            .with_conn(|conn| conn.query_row(sql, [], |row| row.get(0)))
            .unwrap()
            .unwrap();
        assert_eq!(remaining, 0, "a failed create left a {table} row");
    }
    db.with_conn(|conn| conn.execute_batch("DROP TRIGGER reject_conversation"))
        .unwrap()
        .unwrap();
    let id = store.create(temp.path(), "fixture", "fixture").unwrap();
    assert_eq!(
        count(&db, "SELECT COUNT(*) FROM conversations WHERE id=?1", &id),
        1
    );
}

/// A non-contention failure at the message insert must leave the transcript
/// untouched: no message row and no incremented count.
#[test]
fn a_failed_append_leaves_the_transcript_unchanged() {
    let (temp, db, store, _path) = session_fixture();
    let id = store.create(temp.path(), "fixture", "fixture").unwrap();
    db.with_conn(|conn| {
        conn.execute_batch(
            "CREATE TRIGGER reject_message BEFORE INSERT ON messages \
             BEGIN SELECT RAISE(ABORT,'fixture append failure'); END;",
        )
    })
    .unwrap()
    .unwrap();
    let error = store.append(&id, "user", "must not persist").unwrap_err();
    assert!(!deepseek_code_core::db::is_contention(&error), "{error:#}");
    assert!(
        format!("{error:#}").contains("fixture append failure"),
        "{error:#}"
    );
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM messages WHERE conversation_id=?1",
            &id
        ),
        0
    );
    assert_eq!(
        count(
            &db,
            "SELECT message_count FROM conversations WHERE id=?1",
            &id
        ),
        0
    );
    db.with_conn(|conn| conn.execute_batch("DROP TRIGGER reject_message"))
        .unwrap()
        .unwrap();
    store.append(&id, "user", "persisted once").unwrap();
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM messages WHERE conversation_id=?1",
            &id
        ),
        1
    );
}

/// A failure after the last content write happens after the message row and
/// counter were written. A stale trailing timestamp may now be a no-op, so
/// inject at the durable write itself. The whole unit must roll back, leaving
/// no message, counter, content or hook-origin row.
#[test]
fn a_failed_append_rolls_back_after_message_and_counter_writes() {
    let (temp, db, store, _path) = session_fixture();
    let id = store.create(temp.path(), "fixture", "fixture").unwrap();
    db.with_conn(|conn| {
        conn.execute_batch(
            // The structured row is written after the counter. For hooks the
            // order is message, origin, counter. Conditions prove each fault
            // fires only after all of that append's rows have been written;
            // activity timestamp triggers cannot cause an earlier failure.
            "CREATE TRIGGER reject_structured_append \
             AFTER INSERT ON message_user_content \
             WHEN EXISTS (SELECT 1 FROM messages m JOIN conversations c \
               ON c.id=m.conversation_id WHERE m.id=NEW.message_id AND c.message_count=1) \
             BEGIN SELECT RAISE(ABORT,'fixture late append failure'); END; \
             CREATE TRIGGER reject_hook_append \
             AFTER UPDATE OF message_count ON conversations \
             WHEN NEW.message_count > OLD.message_count AND EXISTS \
               (SELECT 1 FROM messages m JOIN peripheral_settings p \
                ON p.key='message-origin:' || m.id WHERE m.conversation_id=NEW.id) \
             BEGIN SELECT RAISE(ABORT,'fixture late append failure'); END;",
        )
    })
    .unwrap()
    .unwrap();
    let content = serde_json::json!([{"type": "text", "text": "structured body"}]);
    let error = store
        .append_user_content(&id, "structured body", &content)
        .unwrap_err();
    assert!(!deepseek_code_core::db::is_contention(&error), "{error:#}");
    assert!(
        format!("{error:#}").contains("fixture late append failure"),
        "{error:#}"
    );
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM messages WHERE conversation_id=?1",
            &id
        ),
        0,
        "the message row must not survive the rolled-back unit"
    );
    assert_eq!(
        count(
            &db,
            "SELECT message_count FROM conversations WHERE id=?1",
            &id
        ),
        0,
        "the counter update must not survive the rolled-back unit"
    );
    assert_eq!(
        scalar(&db, "SELECT COUNT(*) FROM message_user_content"),
        0,
        "the structured content row must not survive the rolled-back unit"
    );
    // The hook append writes its origin row before the counter update, so
    // this proves that side row is rolled back too rather than never written.
    let error = store
        .append_hook_user(&id, "hook must not persist")
        .unwrap_err();
    assert!(!deepseek_code_core::db::is_contention(&error), "{error:#}");
    assert!(
        format!("{error:#}").contains("fixture late append failure"),
        "{error:#}"
    );
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM messages WHERE conversation_id=?1",
            &id
        ),
        0,
        "the hook message row must not survive the rolled-back unit"
    );
    assert_eq!(
        scalar(
            &db,
            "SELECT COUNT(*) FROM peripheral_settings WHERE key LIKE 'message-origin:%'"
        ),
        0,
        "the hook origin row must not survive the rolled-back unit"
    );
    assert_eq!(
        count(
            &db,
            "SELECT message_count FROM conversations WHERE id=?1",
            &id
        ),
        0
    );
    db.with_conn(|conn| {
        conn.execute_batch(
            "DROP TRIGGER reject_structured_append; DROP TRIGGER reject_hook_append;",
        )
    })
    .unwrap()
    .unwrap();
    // The hook-origin side row is written in the same unit; it exists only for a
    // committed append.
    store.append_hook_user(&id, "hook body").unwrap();
    assert_eq!(
        count(
            &db,
            "SELECT COUNT(*) FROM messages WHERE conversation_id=?1",
            &id
        ),
        1
    );
    assert_eq!(
        scalar(
            &db,
            "SELECT COUNT(*) FROM peripheral_settings WHERE key LIKE 'message-origin:%'"
        ),
        1,
        "a committed hook append records exactly one origin row"
    );
    assert_eq!(
        count(
            &db,
            "SELECT message_count FROM conversations WHERE id=?1",
            &id
        ),
        1
    );
}

#[test]
fn turn_write_survives_a_competing_writer_that_releases_within_the_deadline() {
    let (_temp, db, message, path) = turn_fixture();
    let (holder, release) = hold_write_lock(path);
    // Release the lock from another thread as the retry loop is waiting, so the
    // write can only succeed by retrying after contention.
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1500));
        release.send(()).unwrap();
    });
    // busy_timeout is lowered so the retry loop, not one long wait, absorbs the
    // contention; the default 5s would hide whether write_tx retries at all.
    let result = db
        .with_conn(|conn| -> anyhow::Result<()> {
            conn.busy_timeout(Duration::from_millis(50)).unwrap();
            write_tx(conn, "turn results", Duration::from_secs(30), |tx| {
                insert_tool_rows(tx, &message, 200)
            })
        })
        .unwrap();
    result.unwrap();
    releaser.join().unwrap();
    holder.join().unwrap();
    let count: i64 = db
        .with_conn(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM tool_calls WHERE message_id=?1",
                [&message],
                |row| row.get(0),
            )
        })
        .unwrap()
        .unwrap();
    assert_eq!(count, 200);
}

#[test]
fn turn_write_is_all_or_nothing_when_the_deadline_expires() {
    let (_temp, db, message, path) = turn_fixture();
    let (holder, release) = hold_write_lock(path);
    let error = db
        .with_conn(|conn| -> anyhow::Result<()> {
            conn.busy_timeout(Duration::from_millis(50)).unwrap();
            write_tx(conn, "turn results", Duration::from_millis(200), |tx| {
                insert_tool_rows(tx, &message, 200)
            })
        })
        .unwrap()
        .unwrap_err();
    assert!(deepseek_code_core::db::is_contention(&error), "{error:#}");
    assert!(format!("{error:#}").contains("turn results"), "{error:#}");
    release.send(()).unwrap();
    holder.join().unwrap();
    let count: i64 = db
        .with_conn(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM tool_calls WHERE message_id=?1",
                [&message],
                |row| row.get(0),
            )
        })
        .unwrap()
        .unwrap();
    assert_eq!(
        count, 0,
        "a failed turn write must not leave a partial prefix"
    );
}
