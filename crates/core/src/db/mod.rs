mod context_search_index;
pub(crate) mod context_search_repo;
pub mod conversation_repo;
pub mod stored_binding;
pub mod diff_repo;
pub mod h5_repo;
pub mod im_config_repo;
pub mod im_permission_repo;
pub mod message_repo;
pub(crate) mod model_results;
pub mod migration;
pub mod permission_repo;
pub mod project_repo;
pub mod request_repo;
pub mod schema;
#[doc(hidden)]
pub mod session_store;
pub mod task_repo;
pub mod turn_repo;
pub mod transcript;

use anyhow::Result;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::Connection;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Starts a transaction that takes the write lock up front.
///
/// A deferred transaction that reads before writing cannot upgrade its WAL
/// snapshot once another connection commits, so SQLite fails the first write
/// with SQLITE_BUSY at once instead of honouring busy_timeout. IMMEDIATE waits
/// for the writer slot before reading, which keeps busy_timeout effective.
pub fn write_transaction(conn: &Connection) -> rusqlite::Result<rusqlite::Transaction<'_>> {
    rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)
}

pub fn is_contention(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        matches!(cause.downcast_ref::<rusqlite::Error>(),
        Some(rusqlite::Error::SqliteFailure(code, _)) if matches!(code.code,
            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked))
    })
}

/// Extended SQLite result code of the first SQLite failure in the chain.
///
/// The extended code distinguishes a busy writer (`5`) from the extended
/// variants SQLite reports for the same lock; it is safe to expose.
pub fn sqlite_extended_code(error: &anyhow::Error) -> Option<i32> {
    error.chain().find_map(|cause| {
        cause
            .downcast_ref::<rusqlite::Error>()
            .and_then(|error| match error {
                rusqlite::Error::SqliteFailure(code, _) => Some(code.extended_code),
                _ => None,
            })
    })
}

/// Names a write boundary without SQL text, bound values or user content.
///
/// A raw SQLite error carries only "database is locked"; the operation, the
/// session and how long the boundary waited are what let the next interruption
/// be attributed to one caller instead of the whole turn.
pub fn write_boundary_context(
    operation: &str,
    session: &str,
    started: std::time::Instant,
    error: anyhow::Error,
) -> anyhow::Error {
    let code = sqlite_extended_code(&error)
        .map(|code| code.to_string())
        .unwrap_or_else(|| "none".to_owned());
    error.context(format!(
        "{operation} failed after {}ms (session {session}, sqlite extended code {code})",
        started.elapsed().as_millis()
    ))
}

const WRITE_TX_BACKOFF_START: std::time::Duration = std::time::Duration::from_millis(20);
const WRITE_TX_BACKOFF_MAX: std::time::Duration = std::time::Duration::from_millis(400);

/// Startup writes may legitimately wait for another process that is finishing a
/// turn, so they use the same generous bound as session writes.
pub const STARTUP_WRITE_DEADLINE: std::time::Duration =
    std::time::Duration::from_secs(60);

/// Exponential backoff plus up to 25% jitter, derived from the attempt and the
/// calling thread so concurrent writers do not retry in lockstep.
fn write_tx_backoff(attempts: u32) -> std::time::Duration {
    use std::hash::{Hash, Hasher};
    let multiplier = 1u32 << attempts.saturating_sub(1).min(16);
    let base = WRITE_TX_BACKOFF_START
        .saturating_mul(multiplier)
        .min(WRITE_TX_BACKOFF_MAX);
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    attempts.hash(&mut hasher);
    std::thread::current().id().hash(&mut hasher);
    base + base.mul_f64((hasher.finish() % 2_501) as f64 / 10_000.0)
}

/// Human-readable deadline for a lock timeout message.
fn deadline_text(deadline: std::time::Duration) -> String {
    if deadline.as_secs() >= 1 {
        format!("{} s", deadline.as_secs())
    } else {
        format!("{} ms", deadline.as_millis())
    }
}

/// Restores a pooled connection's own `busy_timeout` on every exit path.
///
/// `DbManager` lends shared connections. Lowering the timeout for one bounded
/// poll must not leak into the next borrower, including when the poll unwinds.
pub(crate) struct BusyTimeoutGuard<'a> {
    conn: &'a Connection,
    original_ms: u64,
}

impl<'a> BusyTimeoutGuard<'a> {
    fn capture(conn: &'a Connection) -> Result<Self> {
        let original_ms = conn.query_row("PRAGMA busy_timeout", [], |row| row.get(0))?;
        Ok(Self { conn, original_ms })
    }

    pub(crate) fn within(conn: &'a Connection, wait: std::time::Duration) -> Result<Self> {
        let guard = Self::capture(conn)?;
        conn.busy_timeout(wait)?;
        Ok(guard)
    }
}

impl Drop for BusyTimeoutGuard<'_> {
    fn drop(&mut self) {
        let _ = self
            .conn
            .busy_timeout(std::time::Duration::from_millis(self.original_ms));
    }
}

/// Bound SQLite's lock acquisition and the time between retries by one deadline.
/// The caller may finish uncontended work after it has acquired the writer.
fn retry_contention<T>(
    conn: &Connection,
    label: &str,
    deadline: Duration,
    mut attempt: impl FnMut(Instant) -> Result<T>,
) -> Result<T> {
    let started = Instant::now();
    let until = started + deadline;
    let timeout = BusyTimeoutGuard::capture(conn)?;
    let original = Duration::from_millis(timeout.original_ms);
    let mut attempts = 0;
    loop {
        conn.busy_timeout(original.min(until.saturating_duration_since(Instant::now()) / 2))?;
        attempts += 1;
        match attempt(until) {
            Ok(value) => {
                if attempts > 1 {
                    tracing::warn!(
                        module = "db",
                        label,
                        attempts,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        "write transaction recovered after contention"
                    );
                }
                return Ok(value);
            }
            Err(error) if is_contention(&error) => {
                let remaining = until.saturating_duration_since(Instant::now());
                // Leave room for another probe when SQLite returned BUSY immediately.
                let wait = write_tx_backoff(attempts)
                    .min((remaining / 2).max(Duration::from_millis(1)))
                    .min(remaining);
                if !wait.is_zero() {
                    tracing::warn!(
                        module = "db",
                        label,
                        attempts,
                        waited_ms = started.elapsed().as_millis() as u64,
                        retry_in_ms = wait.as_millis() as u64,
                        "write transaction hit database contention; retrying"
                    );
                    std::thread::sleep(wait);
                }
                if Instant::now() >= until {
                    return Err(error.context(format!(
                        "{label}: database stayed locked by another DeepSeek Code process for {} ({attempts} attempts)",
                        deadline_text(deadline)
                    )));
                }
            }
            Err(error) => return Err(error),
        }
    }
}

// SQLite may wake up and acquire the lock after its last timed sleep.
fn check_acquisition(until: Instant) -> rusqlite::Result<()> {
    if Instant::now() >= until {
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            None,
        ));
    }
    Ok(())
}

/// Each attempt owns one IMMEDIATE transaction. Both the body and commit
/// must be replayable after rollback on SQLite contention.
pub fn write_tx<T>(
    conn: &Connection,
    label: &str,
    deadline: Duration,
    mut f: impl FnMut(&rusqlite::Transaction<'_>) -> Result<T>,
) -> Result<T> {
    write_tx_with_commit(conn, label, deadline, |tx| {
        let value = f(&tx)?;
        tx.commit()?;
        Ok(value)
    })
}

/// For callers that must hold their own locks across commit and publication.
pub(crate) fn write_tx_with_commit<T>(
    conn: &Connection,
    label: &str,
    deadline: Duration,
    mut finish: impl FnMut(rusqlite::Transaction<'_>) -> Result<T>,
) -> Result<T> {
    retry_contention(conn, label, deadline, |until| {
        let tx = write_transaction(conn)?;
        check_acquisition(until)?;
        // Once we own the writer, a later BUSY (including COMMIT) must
        // return to the retry loop. A fresh SQLite wait here could use up
        // the budget a second time after a long transaction body.
        conn.busy_timeout(Duration::ZERO)?;
        finish(tx)
    })
}

pub struct DbManager {
    data_dir: PathBuf,
    request_recording: std::sync::RwLock<
        std::collections::HashMap<String, crate::native_engine::request_recording::Policy>,
    >,
    pool: Pool<SqliteConnectionManager>,
}

impl DbManager {
    pub fn data_dir(&self) -> &std::path::Path {
        &self.data_dir
    }
    pub fn set_request_recording_policy(&self, session: &str, cli: bool, full_access: bool) {
        self.request_recording
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                session.into(),
                crate::native_engine::request_recording::Policy { cli, full_access },
            );
    }
    pub fn request_recording_policy(
        &self,
        session: &str,
    ) -> Option<crate::native_engine::request_recording::Policy> {
        self.request_recording
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(session)
            .cloned()
    }
    pub fn new(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let manager = SqliteConnectionManager::file(&path).with_init(|conn| {
            conn.execute_batch("PRAGMA foreign_keys=ON; PRAGMA busy_timeout=5000;")
        });
        let pool = Pool::builder()
            .max_size(10)
            .min_idle(Some(2))
            .connection_timeout(std::time::Duration::from_secs(5))
            .idle_timeout(Some(std::time::Duration::from_secs(30)))
            .build(manager)?;
        let conn = pool.get()?;
        // SQLite requires journal mode changes outside a transaction.
        retry_contention(&conn, "enabling WAL", STARTUP_WRITE_DEADLINE, |until| {
            conn.execute_batch("PRAGMA journal_mode=WAL;")?;
            check_acquisition(until)?;
            Ok(())
        })?;
        conn.execute_batch("PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;")?;
        Ok(Self {
            pool,
            data_dir: path
                .parent()
                .unwrap_or(std::path::Path::new("."))
                .to_path_buf(),
            request_recording: Default::default(),
        })
    }

    pub fn init(&self) -> Result<()> {
        self.init_with_deadline(STARTUP_WRITE_DEADLINE)
    }

    /// Run the schema bootstrap with an injectable contention deadline.
    ///
    /// The whole batch stays in one IMMEDIATE transaction so concurrent hosts
    /// cannot observe half-migrated tables or upgrade from a stale WAL snapshot.
    /// Every statement is idempotent, so [`write_tx`] can re-run the closure
    /// after rolling back a contended attempt.
    fn init_with_deadline(&self, deadline: std::time::Duration) -> Result<()> {
        self.with_conn(|conn| -> Result<()> {
            write_tx(conn, "schema init", deadline, |tx| {
                tx.execute_batch(schema::SCHEMA_SQL)?;
                context_search_index::migrate(tx)?;
                crate::services::migrate(tx)?;
                crate::services::input_queue::migrate(tx)?;
                conversation_repo::migrate_workspace_mode(tx)?;

                let has_col: i64 = tx
                    .query_row(
                        "SELECT COUNT(*) FROM pragma_table_info('im_configs') WHERE name='connection_type'",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap_or(0);

                if has_col == 0 {
                    tx.execute(
                        "ALTER TABLE im_configs ADD COLUMN connection_type TEXT NOT NULL DEFAULT 'webhook'",
                        [],
                    )?;
                }

                tx.execute("CREATE INDEX IF NOT EXISTS idx_im_configs_platform ON im_configs(platform)", [])?;
                tx.execute("CREATE INDEX IF NOT EXISTS idx_im_configs_status ON im_configs(status)", [])?;
                tx.execute("CREATE INDEX IF NOT EXISTS idx_im_configs_connection_type ON im_configs(connection_type)", [])?;
                tx.execute("CREATE INDEX IF NOT EXISTS idx_im_connections_platform ON im_connections(platform)", [])?;
                tx.execute("CREATE INDEX IF NOT EXISTS idx_im_connections_status ON im_connections(status)", [])?;
                tx.execute("CREATE INDEX IF NOT EXISTS idx_im_connections_type ON im_connections(connection_type)", [])?;
                tx.execute("CREATE INDEX IF NOT EXISTS idx_im_message_stats_platform_date ON im_message_stats(platform, date)", [])?;
                tx.execute("CREATE INDEX IF NOT EXISTS idx_im_message_stats_date ON im_message_stats(date)", [])?;
                tx.execute("CREATE INDEX IF NOT EXISTS idx_im_user_permissions_platform_user ON im_user_permissions(platform, user_id)", [])?;
                tx.execute("CREATE INDEX IF NOT EXISTS idx_im_user_permissions_user_id ON im_user_permissions(user_id)", [])?;
                tx.execute("CREATE INDEX IF NOT EXISTS idx_im_user_permissions_paired_code ON im_user_permissions(paired_code)", [])?;
                tx.execute("CREATE INDEX IF NOT EXISTS idx_im_error_logs_platform ON im_error_logs(platform)", [])?;
                tx.execute("CREATE INDEX IF NOT EXISTS idx_im_error_logs_error_type ON im_error_logs(error_type)", [])?;
                tx.execute("CREATE INDEX IF NOT EXISTS idx_im_error_logs_created_at ON im_error_logs(created_at)", [])?;
                tx.execute("CREATE INDEX IF NOT EXISTS idx_im_sessions_platform ON im_sessions(platform)", [])?;
                tx.execute("CREATE INDEX IF NOT EXISTS idx_im_sessions_user ON im_sessions(user_id)", [])?;
                tx.execute("CREATE INDEX IF NOT EXISTS idx_im_sessions_chat ON im_sessions(chat_id)", [])?;

                let has_session_key: i64 = tx
                    .query_row(
                        "SELECT COUNT(*) FROM pragma_table_info('im_sessions') WHERE name='session_key'",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap_or(0);
                if has_session_key == 0 {
                    tx.execute(
                        "ALTER TABLE im_sessions ADD COLUMN session_key TEXT NOT NULL DEFAULT ''",
                        [],
                    )?;
                }

                // Delivery observations gained a provenance column. Older rows
                // stay 'unknown' and completion treats them conservatively.
                let has_observation_source: i64 = tx
                    .query_row(
                        "SELECT COUNT(*) FROM pragma_table_info('sa_repository_delivery_observations') WHERE name='source'",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap_or(0);
                if has_observation_source == 0 {
                    tx.execute(
                        "ALTER TABLE sa_repository_delivery_observations ADD COLUMN source TEXT NOT NULL DEFAULT 'unknown'",
                        [],
                    )?;
                }

                for col_name in &["thread_id", "message_count", "last_message_content", "metadata", "updated_at"] {
                    // Bug #22 fix: Validate column name to ensure no SQL injection
                    // (col_name comes from hardcoded array, but defensive validation is good practice)
                    debug_assert!(col_name.chars().all(|c| c.is_alphanumeric() || c == '_'),
                        "Invalid column name in migration: {}", col_name);
                    let has_col: i64 = tx
                        .query_row(
                            &format!("SELECT COUNT(*) FROM pragma_table_info('im_sessions') WHERE name='{}'", col_name),
                            [],
                            |row| row.get(0),
                        )
                        .unwrap_or(0);
                    if has_col == 0 {
                        tx.execute(
                            &format!("ALTER TABLE im_sessions ADD COLUMN {} TEXT", col_name),
                            [],
                        )?;
                    }
                }

                Ok(())
            })
        })?
    }

    pub fn with_conn<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&Connection) -> R,
    {
        let conn = self.pool.get()?;
        Ok(f(&conn))
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use rusqlite::Connection;
    use std::sync::mpsc;
    use std::time::Duration;

    /// Hold `path`'s write lock in another thread until the returned sender fires.
    pub(crate) fn hold_write_lock(
        path: std::path::PathBuf,
    ) -> (std::thread::JoinHandle<()>, mpsc::Sender<()>) {
        let (locked_tx, locked_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let holder = std::thread::spawn(move || {
            let conn = Connection::open(path).unwrap();
            conn.execute_batch("BEGIN IMMEDIATE").unwrap();
            locked_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(30)).unwrap();
            conn.execute_batch("ROLLBACK").unwrap();
        });
        locked_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        (holder, release_tx)
    }

    /// Release the writer after `delay`, so the call under test must wait for it.
    pub(crate) fn release_after(
        release: mpsc::Sender<()>,
        holder: std::thread::JoinHandle<()>,
        delay: Duration,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            std::thread::sleep(delay);
            release.send(()).unwrap();
            holder.join().unwrap();
        })
    }
}

#[cfg(test)]
mod write_tx_tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    fn fixture() -> (tempfile::TempDir, DbManager, std::path::PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("write_tx.sqlite");
        let db = DbManager::new(path.clone()).unwrap();
        db.init().unwrap();
        db.with_conn(|conn| conn.execute_batch("CREATE TABLE items(x INTEGER)"))
            .unwrap()
            .unwrap();
        (temp, db, path)
    }

    /// Hold the write lock until the returned sender fires.
    fn hold_write_lock(
        path: std::path::PathBuf,
    ) -> (std::thread::JoinHandle<()>, mpsc::Sender<()>) {
        let (locked_tx, locked_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let holder = std::thread::spawn(move || {
            let conn = Connection::open(path).unwrap();
            conn.execute_batch("BEGIN IMMEDIATE").unwrap();
            locked_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
            conn.execute_batch("ROLLBACK").unwrap();
        });
        locked_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        (holder, release_tx)
    }

    #[test]
    fn contention_is_retried_until_the_writer_releases() {
        let (_temp, db, path) = fixture();
        let (holder, release) = hold_write_lock(path);
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            release.send(()).unwrap();
        });
        let value = db
            .with_conn(|conn| {
                conn.busy_timeout(Duration::from_millis(50)).unwrap();
                write_tx(conn, "retry probe", Duration::from_secs(5), |tx| {
                    tx.execute("INSERT INTO items(x) VALUES (42)", [])?;
                    Ok(42)
                })
            })
            .unwrap()
            .unwrap();
        assert_eq!(value, 42);
        releaser.join().unwrap();
        holder.join().unwrap();
        let count: i64 = db
            .with_conn(|conn| conn.query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0)))
            .unwrap()
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn non_contention_errors_are_not_retried() {
        let (_temp, db, _path) = fixture();
        let mut calls = 0;
        let error = db
            .with_conn(|conn| {
                write_tx(conn, "missing table", Duration::from_secs(5), |tx| {
                    calls += 1;
                    tx.execute("INSERT INTO missing_table(x) VALUES (1)", [])?;
                    Ok(())
                })
            })
            .unwrap()
            .unwrap_err();
        assert_eq!(calls, 1);
        assert!(!is_contention(&error), "{error:#}");
    }

    #[test]
    fn deadline_exhaustion_reports_the_label() {
        let (_temp, db, path) = fixture();
        let (holder, release) = hold_write_lock(path);
        let error = db
            .with_conn(|conn| {
                conn.busy_timeout(Duration::from_millis(50)).unwrap();
                write_tx(conn, "deadline probe", Duration::from_millis(200), |tx| {
                    tx.execute("INSERT INTO items(x) VALUES (1)", [])?;
                    Ok(())
                })
            })
            .unwrap()
            .unwrap_err();
        assert!(is_contention(&error), "{error:#}");
        assert!(format!("{error:#}").contains("deadline probe"), "{error:#}");
        release.send(()).unwrap();
        holder.join().unwrap();
    }

    /// The extended code identifies the failing SQLite condition without
    /// exposing SQL text or bound values.
    #[test]
    fn contention_exposes_the_extended_code() {
        let (_temp, db, path) = fixture();
        let (holder, release) = hold_write_lock(path);
        let error = db
            .with_conn(|conn| {
                conn.busy_timeout(Duration::from_millis(50)).unwrap();
                write_tx(
                    conn,
                    "extended code probe",
                    Duration::from_millis(100),
                    |tx| {
                        tx.execute("INSERT INTO items(x) VALUES (1)", [])?;
                        Ok(())
                    },
                )
            })
            .unwrap()
            .unwrap_err();
        assert_eq!(sqlite_extended_code(&error), Some(5));
        let text = format!("{error:#}");
        assert!(text.contains("extended code probe"), "{text}");
        assert!(!text.contains("INSERT INTO"), "{text}");
        release.send(()).unwrap();
        holder.join().unwrap();
    }

    #[test]
    fn a_failed_transaction_leaves_no_partial_rows() {
        let (_temp, db, _path) = fixture();
        let error = db
            .with_conn(|conn| {
                write_tx(
                    conn,
                    "rollback probe",
                    Duration::from_secs(5),
                    |tx| -> Result<()> {
                        tx.execute("INSERT INTO items(x) VALUES (7)", [])?;
                        anyhow::bail!("intentional failure")
                    },
                )
            })
            .unwrap()
            .unwrap_err();
        assert!(format!("{error:#}").contains("intentional failure"));
        let count: i64 = db
            .with_conn(|conn| conn.query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0)))
            .unwrap()
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn init_waits_for_a_writer_that_outlasts_the_busy_timeout() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("init_wait.sqlite");
        // Put the fresh, schema-less file in WAL mode before the holder starts,
        // so creating the pool cannot race the holder's write lock.
        Connection::open(&path)
            .unwrap()
            .execute_batch("PRAGMA journal_mode=WAL;")
            .unwrap();
        let (holder, release) = hold_write_lock(path.clone());
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(7));
            release.send(()).unwrap();
        });
        let started = std::time::Instant::now();
        let db = DbManager::new(path).unwrap();
        db.init().unwrap();
        assert!(
            started.elapsed() >= Duration::from_secs(6),
            "init returned before the holder released the writer: {:?}",
            started.elapsed()
        );
        releaser.join().unwrap();
        holder.join().unwrap();
        let tables: i64 = db
            .with_conn(|conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM sqlite_schema WHERE type='table' AND name='conversations'",
                    [],
                    |row| row.get(0),
                )
            })
            .unwrap()
            .unwrap();
        assert_eq!(tables, 1);
    }

    #[test]
    fn init_deadline_reports_a_database_locked_by_another_process() {
        let (_temp, db, path) = fixture();
        let (holder, release) = hold_write_lock(path);
        let started = std::time::Instant::now();
        let error = db
            .init_with_deadline(Duration::from_millis(300))
            .unwrap_err();
        let elapsed = started.elapsed();
        let text = format!("{error:#}");
        assert!(is_contention(&error), "{text}");
        assert!(
            text.contains("database stayed locked by another DeepSeek Code process"),
            "{text}"
        );
        assert!(text.contains("for 300 ms"), "{text}");
        assert!(elapsed < Duration::from_secs(5), "returned after {elapsed:?}");
        release.send(()).unwrap();
        holder.join().unwrap();
    }

    #[test]
    fn commit_contention_uses_only_the_remaining_budget() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("reader.sqlite");
        let writer = Connection::open(&path).unwrap();
        writer.execute_batch("CREATE TABLE items(x)").unwrap();
        let reader = Connection::open(&path).unwrap();
        // In DELETE mode a reader permits BEGIN IMMEDIATE but blocks COMMIT.
        reader.execute_batch("BEGIN; SELECT * FROM items").unwrap();
        let slow_write = |tx: &rusqlite::Transaction<'_>| -> Result<()> {
            tx.execute("INSERT INTO items VALUES(1)", [])?;
            std::thread::sleep(Duration::from_millis(150));
            Ok(())
        };
        let budget = Duration::from_millis(100);
        writer.busy_timeout(Duration::from_secs(4)).unwrap();
        let started = Instant::now();
        let error = write_tx(&writer, "commit probe", budget, slow_write).unwrap_err();
        assert!(is_contention(&error));
        assert!(started.elapsed() < Duration::from_secs(2));
        reader.execute_batch("ROLLBACK").unwrap();
        // A long transaction body can finish normally when COMMIT is uncontended.
        write_tx(&writer, "commit probe", budget, slow_write).unwrap();
        assert_eq!(
            writer
                .query_row("SELECT count(*) FROM items", [], |r| r.get::<_, u32>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn writer_release_on_either_side_of_deadline_restores_busy_timeout() {
        let (_temp, db, path) = fixture();
        let deadline = Duration::from_millis(800);
        for (delay, should_succeed) in [(700, true), (900, false)] {
            db.with_conn(|conn| {
                conn.busy_timeout(Duration::from_millis(2345)).unwrap();
                let (holder, release) = hold_write_lock(path.clone());
                let releaser =
                    test_support::release_after(release, holder, Duration::from_millis(delay));
                let result = write_tx(conn, "deadline edge", deadline, |tx| {
                    tx.execute("INSERT INTO items VALUES(1)", [])?;
                    Ok(())
                });
                releaser.join().unwrap();
                let timeout: u64 = conn
                    .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
                    .unwrap();
                assert_eq!(timeout, 2345);
                if should_succeed {
                    result.unwrap();
                } else {
                    let error = result.unwrap_err();
                    assert_eq!(sqlite_extended_code(&error), Some(5));
                    assert!(
                        error
                            .to_string()
                            .contains("locked by another DeepSeek Code process"),
                        "{error:#}"
                    );
                }
            })
            .unwrap();
        }
        assert_eq!(
            db.with_conn(
                |conn| conn.query_row("SELECT count(*) FROM items", [], |row| row.get::<_, u32>(0))
            )
            .unwrap()
            .unwrap(),
            1
        );
    }

    #[test]
    fn first_open_waits_for_writer_in_default_journal_mode() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("cold.sqlite");
        let journal = |conn: &Connection| {
            conn.query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))
        };
        assert_eq!(
            journal(&Connection::open(&path).unwrap()).unwrap(),
            "delete"
        );
        let (holder, release) = hold_write_lock(path.clone());
        let releaser = test_support::release_after(release, holder, Duration::from_secs(6));
        let db = DbManager::new(path).unwrap();
        db.init().unwrap();
        assert_eq!(db.with_conn(journal).unwrap().unwrap(), "wal");
        releaser.join().unwrap();
    }

    #[test]
    fn repeated_init_is_idempotent() {
        let (_temp, db, _path) = fixture();
        db.init().unwrap();
        db.init().unwrap();
    }
}
