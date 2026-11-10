use deepseek_code_core::db::{message_repo, write_tx};
use deepseek_code_core::runtime::{RuntimePaths, SessionStore, DATABASE_FILE};
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
    let (locked_tx, locked_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let holder = std::thread::spawn(move || {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch("BEGIN IMMEDIATE").unwrap();
        locked_tx.send(()).unwrap();
        release_rx.recv_timeout(Duration::from_secs(30)).unwrap();
        conn.execute_batch("ROLLBACK").unwrap();
    });
    locked_rx.recv_timeout(Duration::from_secs(10)).unwrap();
    (holder, release_tx)
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
