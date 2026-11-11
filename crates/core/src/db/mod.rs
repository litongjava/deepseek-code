pub mod conversation_repo;
pub mod diff_repo;
pub mod h5_repo;
pub mod im_config_repo;
pub mod im_permission_repo;
pub mod message_repo;
pub mod migration;
pub mod permission_repo;
pub mod project_repo;
pub mod request_repo;
pub mod schema;
pub mod task_repo;
pub mod turn_repo;

use anyhow::Result;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::Connection;
use std::path::PathBuf;

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

/// Retries a write transaction on transient SQLite contention.
///
/// The closure runs inside one IMMEDIATE transaction and must be re-runnable:
/// every retry rolls back and calls it again from the start. Only contention
/// is retried; any other error returns immediately.
pub fn write_tx<T>(
    conn: &Connection,
    label: &str,
    deadline: std::time::Duration,
    mut f: impl FnMut(&rusqlite::Transaction<'_>) -> Result<T>,
) -> Result<T> {
    let started = std::time::Instant::now();
    let mut attempts: u32 = 1;
    loop {
        // Opening the transaction can itself hit SQLITE_BUSY, and so can COMMIT.
        let outcome = (|| -> Result<T> {
            let tx = write_transaction(conn)?;
            let value = f(&tx)?;
            tx.commit()?;
            Ok(value)
        })();
        match outcome {
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
                let elapsed = started.elapsed();
                if elapsed >= deadline {
                    return Err(error.context(format!(
                        "{label}: database busy, {attempts} attempts over {}ms",
                        elapsed.as_millis()
                    )));
                }
                let wait = write_tx_backoff(attempts);
                tracing::warn!(
                    module = "db",
                    label,
                    attempts,
                    waited_ms = elapsed.as_millis() as u64,
                    retry_in_ms = wait.as_millis() as u64,
                    "write transaction hit database contention; retrying"
                );
                std::thread::sleep(wait);
                attempts += 1;
            }
            Err(error) => return Err(error),
        }
    }
}

pub struct DbManager {
    pool: Pool<SqliteConnectionManager>,
}

impl DbManager {
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
        // journal_mode can report SQLITE_BUSY immediately during first-open races,
        // even with busy_timeout. Retry only this idempotent initialization step.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            match conn.execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;",
            ) {
                Ok(()) => break,
                Err(rusqlite::Error::SqliteFailure(error, _))
                    if matches!(
                        error.code,
                        rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                    ) && std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(20))
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(Self { pool })
    }

    pub fn init(&self) -> Result<()> {
        self.with_conn(|conn| {
            // Acquire the writer before schema reads; concurrent hosts must not upgrade a stale WAL snapshot.
            conn.execute_batch("BEGIN IMMEDIATE")?;

            let result: Result<()> = (|| {
                conn.execute_batch(schema::SCHEMA_SQL)?;
                crate::services::migrate(conn)?;
                crate::services::input_queue::migrate(conn)?;
                conversation_repo::migrate_workspace_mode(conn)?;

                let has_col: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM pragma_table_info('im_configs') WHERE name='connection_type'",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap_or(0);

                if has_col == 0 {
                    conn.execute(
                        "ALTER TABLE im_configs ADD COLUMN connection_type TEXT NOT NULL DEFAULT 'webhook'",
                        [],
                    )?;
                }

                conn.execute("CREATE INDEX IF NOT EXISTS idx_im_configs_platform ON im_configs(platform)", [])?;
                conn.execute("CREATE INDEX IF NOT EXISTS idx_im_configs_status ON im_configs(status)", [])?;
                conn.execute("CREATE INDEX IF NOT EXISTS idx_im_configs_connection_type ON im_configs(connection_type)", [])?;
                conn.execute("CREATE INDEX IF NOT EXISTS idx_im_connections_platform ON im_connections(platform)", [])?;
                conn.execute("CREATE INDEX IF NOT EXISTS idx_im_connections_status ON im_connections(status)", [])?;
                conn.execute("CREATE INDEX IF NOT EXISTS idx_im_connections_type ON im_connections(connection_type)", [])?;
                conn.execute("CREATE INDEX IF NOT EXISTS idx_im_message_stats_platform_date ON im_message_stats(platform, date)", [])?;
                conn.execute("CREATE INDEX IF NOT EXISTS idx_im_message_stats_date ON im_message_stats(date)", [])?;
                conn.execute("CREATE INDEX IF NOT EXISTS idx_im_user_permissions_platform_user ON im_user_permissions(platform, user_id)", [])?;
                conn.execute("CREATE INDEX IF NOT EXISTS idx_im_user_permissions_user_id ON im_user_permissions(user_id)", [])?;
                conn.execute("CREATE INDEX IF NOT EXISTS idx_im_user_permissions_paired_code ON im_user_permissions(paired_code)", [])?;
                conn.execute("CREATE INDEX IF NOT EXISTS idx_im_error_logs_platform ON im_error_logs(platform)", [])?;
                conn.execute("CREATE INDEX IF NOT EXISTS idx_im_error_logs_error_type ON im_error_logs(error_type)", [])?;
                conn.execute("CREATE INDEX IF NOT EXISTS idx_im_error_logs_created_at ON im_error_logs(created_at)", [])?;
                conn.execute("CREATE INDEX IF NOT EXISTS idx_im_sessions_platform ON im_sessions(platform)", [])?;
                conn.execute("CREATE INDEX IF NOT EXISTS idx_im_sessions_user ON im_sessions(user_id)", [])?;
                conn.execute("CREATE INDEX IF NOT EXISTS idx_im_sessions_chat ON im_sessions(chat_id)", [])?;

                let has_session_key: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM pragma_table_info('im_sessions') WHERE name='session_key'",
                        [],
                        |row| row.get(0),
                    )
                    .unwrap_or(0);
                if has_session_key == 0 {
                    conn.execute(
                        "ALTER TABLE im_sessions ADD COLUMN session_key TEXT NOT NULL DEFAULT ''",
                        [],
                    )?;
                }

                for col_name in &["thread_id", "message_count", "last_message_content", "metadata", "updated_at"] {
                    // Bug #22 fix: Validate column name to ensure no SQL injection
                    // (col_name comes from hardcoded array, but defensive validation is good practice)
                    debug_assert!(col_name.chars().all(|c| c.is_alphanumeric() || c == '_'),
                        "Invalid column name in migration: {}", col_name);
                    let has_col: i64 = conn
                        .query_row(
                            &format!("SELECT COUNT(*) FROM pragma_table_info('im_sessions') WHERE name='{}'", col_name),
                            [],
                            |row| row.get(0),
                        )
                        .unwrap_or(0);
                    if has_col == 0 {
                        conn.execute(
                            &format!("ALTER TABLE im_sessions ADD COLUMN {} TEXT", col_name),
                            [],
                        )?;
                    }
                }

                Ok(())
            })();

            match result {
                Ok(()) => {
                    conn.execute_batch("COMMIT")?;
                    Ok(()) as Result<()>
                }
                Err(e) => {
                    let _ = conn.execute_batch("ROLLBACK");
                    Err(e) as Result<()>
                }
            }
        })??;
        Ok(())
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
}
