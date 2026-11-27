//! Versioned registry store: an independent SQLite database plus a private
//! `domain-id` marker. Nothing here touches the conversation database.
//!
//! Bootstrap is crash-conservative: the marker is created once (create-new,
//! synced) before the database exists, the schema and matching domain row are
//! committed in one immediate transaction under `schema.lock`, and any
//! inconsistent marker/database pair is reported instead of repaired.
#![cfg_attr(not(unix), allow(dead_code))]

#[cfg(unix)]
use super::paths;
use super::{
    types::{
        is_uuid, Activity, CoordResult, CoordinationError, Lifecycle, NativePath, OwnerRecord,
        ParticipantKind, ParticipantRole, Registration,
    },
    CoordinationPaths,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior};
#[cfg(unix)]
use std::{
    ffi::OsStr,
    fs::File,
    io::{Read, Write},
};
use std::{
    path::Path,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const SCHEMA_SQL: &str = include_str!("schema_v1.sql");
const SCHEMA_VERSION: u32 = 1;
const DOMAIN_MARKER: &str = "domain-id";
const SCHEMA_LOCK: &str = "schema.lock";
const DATABASE_NAME: &str = "coordination.sqlite3";
const WAL_NAME: &str = "coordination.sqlite3-wal";
const SHM_NAME: &str = "coordination.sqlite3-shm";
const JOURNAL_NAME: &str = "coordination.sqlite3-journal";
const MARKER_PROBE_BYTES: u64 = 37;
const HEADER_PROBE_BYTES: usize = 100;
// Schema lock holders may synchronize several files during first open.
const SCHEMA_LOCK_TIMEOUT: Duration = Duration::from_secs(15);
// A peer can hold a SQLite lock while a durable commit waits on storage.
const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(2);
// WAL setup retries the mode change within its own window, with short attempts.
const WAL_MODE_RETRY_TIMEOUT: Duration = Duration::from_secs(2);
const WAL_MODE_BUSY_TIMEOUT: Duration = Duration::from_millis(100);
const MAX_ROWS: usize = 100;

/// One registry connection. Not `Sync`: callers serialize their own access.
pub struct Store {
    connection: Connection,
    paths: CoordinationPaths,
    domain_id: String,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

fn invalid(message: impl Into<String>) -> CoordinationError {
    CoordinationError::InvalidRecord(message.into())
}

fn schema_version(connection: &Connection) -> CoordResult<u32> {
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    u32::try_from(version).map_err(|_| CoordinationError::Corrupt)
}

fn domain_row(connection: &Connection) -> CoordResult<Option<String>> {
    connection
        .query_row(
            "SELECT domain_id FROM domain_meta WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(CoordinationError::from)
}

/// Confirm the schema identity before any mutation or read.
fn check_schema(connection: &Connection, domain_id: &str) -> CoordResult<()> {
    let version = schema_version(connection)?;
    if version > SCHEMA_VERSION {
        return Err(CoordinationError::UnsupportedSchema(version));
    }
    if version != SCHEMA_VERSION {
        return Err(CoordinationError::Corrupt);
    }
    match domain_row(connection)? {
        Some(row) if row == domain_id => Ok(()),
        Some(_) => Err(CoordinationError::DomainMismatch),
        None => Err(CoordinationError::DomainMismatch),
    }
}

impl Store {
    /// Open or initialize the writable registry under `paths.root`.
    ///
    /// New files are created only for a genuinely absent root, under one held
    /// `schema.lock`. Any existing marker/database evidence is inspected
    /// without creating, recovering or checkpointing anything.
    pub fn open_rw(paths: &CoordinationPaths) -> CoordResult<Self> {
        #[cfg(not(unix))]
        {
            let _ = paths;
            return Err(CoordinationError::UnsupportedPlatform);
        }
        #[cfg(unix)]
        {
            Self::open_rw_with(paths, SCHEMA_LOCK_TIMEOUT, bootstrap)
        }
    }

    #[cfg(unix)]
    fn open_rw_with(
        paths: &CoordinationPaths,
        lock_timeout: Duration,
        initialize: impl FnOnce(&File, &CoordinationPaths) -> CoordResult<Self>,
    ) -> CoordResult<Self> {
        let root = match paths::open_private_dir(&paths.root) {
            Ok(root) => root,
            Err(error) if is_missing(&error) => {
                paths.prepare_private().map_err(|error| {
                    CoordinationError::Io(std::io::Error::other(error.to_string()))
                })?;
                paths::open_private_dir(&paths.root).map_err(anyhow_error)?
            }
            Err(error) => return Err(anyhow_error(error)),
        };
        let _lock = acquire_schema_lock(&root, lock_timeout)?;
        match inspect_state(&root, paths)? {
            DiskState::Absent => initialize(&root, paths),
            DiskState::Existing { domain_id } => open_existing(paths, &domain_id),
        }
    }

    pub fn domain_id(&self) -> &str {
        &self.domain_id
    }

    /// Read-only view. `None` means no coordination state exists at all.
    ///
    /// The reader never creates, recovers, migrates or checkpoints anything: an
    /// initialized store must already carry its private WAL/SHM pair.
    pub fn open_read(paths: &CoordinationPaths) -> CoordResult<Option<Self>> {
        #[cfg(not(unix))]
        {
            let _ = paths;
            return Err(CoordinationError::UnsupportedPlatform);
        }
        #[cfg(unix)]
        {
            let root = match paths::open_private_dir(&paths.root) {
                Ok(root) => root,
                Err(error) if is_missing(&error) => return Ok(None),
                Err(error) => return Err(anyhow_error(error)),
            };
            match inspect_state(&root, paths)? {
                DiskState::Absent => Ok(None),
                DiskState::Existing { domain_id } => {
                    let connection = open_validated_read(&paths.database(), &domain_id)?;
                    Ok(Some(Self {
                        connection,
                        paths: paths.clone(),
                        domain_id,
                    }))
                }
            }
        }
    }

    #[allow(dead_code)] // Read-only owner/queue queries use it from round 03 onwards.
    pub(super) fn connection(&self) -> &Connection {
        &self.connection
    }

    #[allow(dead_code)] // Used by registry/queue rounds.
    pub(super) fn paths(&self) -> &CoordinationPaths {
        &self.paths
    }
}

/// What a held private root currently contains.
enum DiskState {
    /// No marker and no main database: only an empty root or a bare lock.
    Absent,
    Existing {
        domain_id: String,
    },
}

/// Basenames seen directly under the root.
#[derive(Debug, PartialEq, Eq)]
enum RootEntries {
    Empty,
    LockOnly,
    Other,
}

/// Convert a bounded inspection failure into a store category.
fn state_error(error: std::io::Error) -> CoordinationError {
    match error.kind() {
        std::io::ErrorKind::InvalidData => CoordinationError::Corrupt,
        _ => CoordinationError::Io(error),
    }
}

/// Map an `anyhow` helper failure, keeping a genuine ENOENT detectable.
#[cfg(unix)]
fn anyhow_error(error: anyhow::Error) -> CoordinationError {
    if error.downcast_ref::<rustix::io::Errno>() == Some(&rustix::io::Errno::NOENT) {
        CoordinationError::Io(std::io::Error::from(std::io::ErrorKind::NotFound))
    } else {
        CoordinationError::Io(std::io::Error::other(error.to_string()))
    }
}

/// True only for a genuine missing path component.
#[cfg(unix)]
fn is_missing(error: &anyhow::Error) -> bool {
    error.downcast_ref::<rustix::io::Errno>() == Some(&rustix::io::Errno::NOENT)
}

/// Read the pinned root's entry names without following anything.
#[cfg(unix)]
fn scan_root(root: &File) -> CoordResult<RootEntries> {
    use rustix::fs::Dir;
    let entries = Dir::read_from(root).map_err(|error| CoordinationError::Io(error.into()))?;
    let mut seen_lock = false;
    let mut seen_any = false;
    for entry in entries {
        let entry = entry.map_err(|error| CoordinationError::Io(error.into()))?;
        let name = entry.file_name();
        let bytes = name.to_bytes();
        if bytes == b"." || bytes == b".." {
            continue;
        }
        seen_any = true;
        if bytes != SCHEMA_LOCK.as_bytes() {
            return Ok(RootEntries::Other);
        }
        seen_lock = true;
    }
    Ok(match (seen_any, seen_lock) {
        (false, _) => RootEntries::Empty,
        (true, true) => RootEntries::LockOnly,
        (true, false) => RootEntries::Other,
    })
}
/// Read `domain-id` through the private, bounded, no-follow path.
///
/// `Ok(None)` means the entry is genuinely absent; every other malformed case
/// is `Corrupt` and the bytes are left untouched.
#[cfg(unix)]
fn read_marker(root: &File) -> CoordResult<Option<String>> {
    let file = match paths::open_existing_private_at(root, OsStr::new(DOMAIN_MARKER), false) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(state_error(error)),
    };
    let mut bytes = Vec::new();
    file.take(MARKER_PROBE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(CoordinationError::Io)?;
    if bytes.len() != 36 {
        return Err(CoordinationError::Corrupt);
    }
    let text = std::str::from_utf8(&bytes).map_err(|_| CoordinationError::Corrupt)?;
    if !is_uuid(text) {
        return Err(CoordinationError::Corrupt);
    }
    Ok(Some(text.to_string()))
}

/// Validate a SQLite-owned state entry using metadata only.
///
/// No descriptor is opened: closing an independent descriptor for an inode can
/// clear this process's SQLite POSIX locks, so inspection never does that.
#[cfg(unix)]
fn check_private_metadata_at(root: &File, name: &str) -> std::io::Result<()> {
    use rustix::fs::{statat, AtFlags, FileType};
    let stat =
        statat(root, OsStr::new(name), AtFlags::SYMLINK_NOFOLLOW).map_err(std::io::Error::from)?;
    let mode = (stat.st_mode as u32) & 0o7777;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile
        || stat.st_uid != rustix::process::geteuid().as_raw()
        || mode != 0o600
    {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidData));
    }
    Ok(())
}

/// The physical main file must carry a complete WAL-mode SQLite header.
///
/// Read through SQLite's own VFS file, so no extra descriptor is opened. The
/// header's `user_version` field is not trusted: the authoritative version may
/// still be committed in the WAL, so it is checked with SQL afterwards.
#[cfg(unix)]
fn check_main_header(connection: &Connection) -> CoordResult<()> {
    use rusqlite::ffi;
    let mut file: *mut ffi::sqlite3_file = std::ptr::null_mut();
    let mut header = [0u8; HEADER_PROBE_BYTES];
    // SAFETY: this borrowed connection owns the VFS file and its method table
    // for the duration of the call, no other thread uses the connection
    // concurrently, and the out-pointer and buffer stay valid and writable.
    // No ownership is transferred and the file is never closed here.
    unsafe {
        let rc = ffi::sqlite3_file_control(
            connection.handle(),
            c"main".as_ptr(),
            ffi::SQLITE_FCNTL_FILE_POINTER,
            (&mut file as *mut *mut ffi::sqlite3_file).cast(),
        );
        if rc != ffi::SQLITE_OK || file.is_null() || (*file).pMethods.is_null() {
            return Err(CoordinationError::Corrupt);
        }
        let read = (*(*file).pMethods)
            .xRead
            .ok_or(CoordinationError::Corrupt)?;
        let rc = read(file, header.as_mut_ptr().cast(), header.len() as _, 0);
        if rc == ffi::SQLITE_IOERR_SHORT_READ {
            return Err(CoordinationError::Corrupt);
        }
        if rc != ffi::SQLITE_OK {
            return Err(rusqlite::Error::SqliteFailure(ffi::Error::new(rc), None).into());
        }
    }
    if &header[..16] != b"SQLite format 3\0" || header[18] != 2 || header[19] != 2 {
        return Err(CoordinationError::Corrupt);
    }
    Ok(())
}

/// A recognized SQLite sidecar must already be a private regular file.
///
/// A missing or unsafe sidecar is corruption of an otherwise initialized
/// domain; only a real permission/I/O failure stays a plain I/O error.
#[cfg(unix)]
fn check_sidecar(root: &File, name: &str) -> CoordResult<()> {
    match check_private_metadata_at(root, name) {
        Ok(()) => Ok(()),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidData
            ) =>
        {
            Err(CoordinationError::Corrupt)
        }
        Err(error) => Err(state_error(error)),
    }
}

/// An initialized domain always carries its private schema lock.
#[cfg(unix)]
fn check_existing_lock(root: &File) -> CoordResult<()> {
    check_private_metadata_at(root, SCHEMA_LOCK).map_err(state_error)
}

/// Inspect an existing private root once, without creating or repairing state.
///
/// SQLite-owned entries are inspected by metadata only; the database header is
/// read later through SQLite's own descriptor.
#[cfg(unix)]
fn inspect_state(root: &File, _paths: &CoordinationPaths) -> CoordResult<DiskState> {
    // Any rollback journal at all is unsupported recovery evidence. It is
    // refused by metadata, so no journal descriptor is ever opened.
    match check_private_metadata_at(root, JOURNAL_NAME) {
        Ok(()) => return Err(CoordinationError::Corrupt),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(CoordinationError::Corrupt),
    }
    let marker = read_marker(root)?;
    let main = check_private_metadata_at(root, DATABASE_NAME);
    match (marker, main) {
        (Some(domain_id), Ok(())) => {
            check_existing_lock(root)?;
            check_sidecar(root, WAL_NAME)?;
            check_sidecar(root, SHM_NAME)?;
            Ok(DiskState::Existing { domain_id })
        }
        (Some(_), Err(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(CoordinationError::MissingDatabase)
        }
        (Some(_), Err(error)) => Err(state_error(error)),
        (None, main) => {
            if main.is_ok() {
                return Err(CoordinationError::DomainMismatch);
            }
            match scan_root(root)? {
                RootEntries::Empty => Ok(DiskState::Absent),
                RootEntries::LockOnly => {
                    check_existing_lock(root)?;
                    Ok(DiskState::Absent)
                }
                RootEntries::Other => Err(CoordinationError::Corrupt),
            }
        }
    }
}
/// Acquire the stable schema lock, creating it only in a genuinely empty root.
#[cfg(unix)]
fn acquire_schema_lock(root: &File, timeout: Duration) -> CoordResult<crate::fs::OwnedFileLock> {
    match paths::open_existing_private_at(root, OsStr::new(SCHEMA_LOCK), true) {
        Ok(file) => return lock_file(file, timeout),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(state_error(error)),
    }
    match scan_root(root)? {
        RootEntries::Empty => {
            let file = paths::create_private_at(root, OsStr::new(SCHEMA_LOCK), false)
                .map_err(anyhow_error)?;
            lock_file(file, timeout)
        }
        RootEntries::LockOnly => {
            let file = paths::open_existing_private_at(root, OsStr::new(SCHEMA_LOCK), true)
                .map_err(state_error)?;
            lock_file(file, timeout)
        }
        RootEntries::Other => {
            // A concurrent cooperative bootstrap may have just created the lock;
            // only an already validated, acquired lock lets this call continue.
            match paths::open_existing_private_at(root, OsStr::new(SCHEMA_LOCK), true) {
                Ok(file) => lock_file(file, timeout),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    Err(CoordinationError::Corrupt)
                }
                Err(error) => Err(state_error(error)),
            }
        }
    }
}

/// Bounded ownership wait on the schema lock.
#[cfg(unix)]
fn lock_file(file: File, timeout: Duration) -> CoordResult<crate::fs::OwnedFileLock> {
    let deadline = Instant::now() + timeout;
    loop {
        match crate::fs::OwnedFileLock::try_acquire(file.try_clone()?)? {
            Some(lock) => return Ok(lock),
            None => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(CoordinationError::Busy);
                }
                std::thread::sleep(remaining.min(Duration::from_millis(10)));
            }
        }
    }
}

/// Retain the WAL and SHM files when the last writer closes.
///
/// Without this, SQLite checkpoints and unlinks the pair, which would make a
/// subsequent read create files. A failed file control is an error.
fn persist_wal(connection: &Connection) -> CoordResult<()> {
    let mut enabled: std::ffi::c_int = 1;
    // SAFETY: the connection is alive and used on its owning thread; `main` is
    // a static C string and `enabled` stays writable for the call.
    let rc = unsafe {
        rusqlite::ffi::sqlite3_file_control(
            connection.handle(),
            c"main".as_ptr(),
            rusqlite::ffi::SQLITE_FCNTL_PERSIST_WAL,
            (&mut enabled as *mut std::ffi::c_int).cast(),
        )
    };
    if rc != rusqlite::ffi::SQLITE_OK {
        return Err(rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(rc), None).into());
    }
    Ok(())
}

/// Open the existing database read-only and verify its schema identity once.
#[cfg(unix)]
fn open_validated_read(database: &Path, domain_id: &str) -> CoordResult<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_NOFOLLOW
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let connection = Connection::open_with_flags(database, flags)?;
    connection.busy_timeout(SQLITE_BUSY_TIMEOUT)?;
    // Header check first: a corrupt main file must not reach any SQL.
    check_main_header(&connection)?;
    let transaction = connection.unchecked_transaction()?;
    check_schema(&transaction, domain_id)?;
    transaction.commit()?;
    Ok(connection)
}
/// Open the existing database for writing without CREATE, and verify identity
/// through a read snapshot first so a rejected store is never modified.
#[cfg(unix)]
fn open_existing(paths: &CoordinationPaths, domain_id: &str) -> CoordResult<Store> {
    drop(open_validated_read(&paths.database(), domain_id)?);
    let connection = open_writer(&paths.database(), false)?;
    check_main_header(&connection)?;
    Ok(Store {
        connection,
        paths: paths.clone(),
        domain_id: domain_id.to_string(),
    })
}

/// Open a writable connection with the required flags and configuration.
fn open_writer(database: &Path, bootstrap: bool) -> CoordResult<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_NOFOLLOW
        | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let connection = Connection::open_with_flags(database, flags)?;
    // Must happen before any query on this connection.
    persist_wal(&connection)?;
    if bootstrap {
        connection.busy_timeout(WAL_MODE_BUSY_TIMEOUT)?;
        configure_wal(&connection)?;
    }
    connection.busy_timeout(SQLITE_BUSY_TIMEOUT)?;
    connection.execute_batch("PRAGMA synchronous=FULL; PRAGMA foreign_keys=ON;")?;
    Ok(connection)
}

/// Switch a freshly created file to WAL, tolerating a first-open race only.
fn configure_wal(connection: &Connection) -> CoordResult<()> {
    let deadline = Instant::now() + WAL_MODE_RETRY_TIMEOUT;
    loop {
        match connection.query_row("PRAGMA journal_mode=WAL", [], |row| row.get::<_, String>(0)) {
            Ok(mode) if mode.eq_ignore_ascii_case("wal") => return Ok(()),
            Ok(_) => return Err(CoordinationError::Corrupt),
            Err(rusqlite::Error::SqliteFailure(code, _))
                if matches!(
                    code.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                ) && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// Create-new marker, synced, before the database file exists. Never replaces.
#[cfg(unix)]
fn create_marker(root: &File) -> CoordResult<String> {
    let domain_id = uuid::Uuid::new_v4().to_string();
    let mut file =
        paths::create_private_at(root, OsStr::new(DOMAIN_MARKER), true).map_err(anyhow_error)?;
    file.write_all(domain_id.as_bytes())?;
    file.sync_all()?;
    drop(file);
    // Durable in the directory before the database appears.
    root.sync_all()?;
    Ok(domain_id)
}

/// The only path that creates state: marker, private database, then schema and
/// domain row in one immediate transaction.
#[cfg(unix)]
fn bootstrap(root: &File, paths: &CoordinationPaths) -> CoordResult<Store> {
    let domain_id = create_marker(root)?;
    let database = paths.database();
    let file =
        paths::create_private_at(root, OsStr::new(DATABASE_NAME), true).map_err(anyhow_error)?;
    file.sync_all()?;
    drop(file);
    let mut connection = open_writer(&database, true)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_SQL)?;
    transaction.execute(
        "INSERT INTO domain_meta(singleton, domain_id, created_ms) VALUES(1, ?1, ?2)",
        rusqlite::params![domain_id, now_ms()],
    )?;
    transaction.commit()?;
    check_schema(&connection, &domain_id)?;
    // A fresh store must already carry its private pair; no chmod or retry.
    check_sidecar(root, WAL_NAME)?;
    check_sidecar(root, SHM_NAME)?;
    Ok(Store {
        connection,
        paths: paths.clone(),
        domain_id,
    })
}
impl Store {
    /// One immediate transaction. The schema identity is rechecked inside the
    /// transaction, and nothing commits unless the closure returns `Ok`.
    pub(super) fn write<T>(
        &mut self,
        f: impl FnOnce(&rusqlite::Transaction<'_>) -> CoordResult<T>,
    ) -> CoordResult<T> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        check_schema(&transaction, &self.domain_id)?;
        let value = f(&transaction)?;
        transaction.commit()?;
        Ok(value)
    }

    /// Insert one open registration. Duplicate instance IDs are rejected.
    pub(crate) fn insert(&mut self, row: &Registration) -> CoordResult<()> {
        row.validate()?;
        let owner_json = serde_json::to_string(&row.owner)
            .map_err(|_| invalid("owner record is not serializable"))?;
        self.write(|transaction| {
            transaction
                .execute(
                    "INSERT INTO instances(
                        instance_id, data_namespace_id, session_id, kind, role,
                        parent_instance_id, worker_id, job_id, owner_json,
                        workspace_encoding, workspace_path, repo_id, worktree_id,
                        branch_ref, activity, activity_since_ms, started_ms,
                        heartbeat_ms, last_git_observed_ms, lifecycle)
                     VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)",
                    rusqlite::params![
                        row.instance_id,
                        row.data_namespace_id,
                        row.session_id,
                        row.kind.as_str(),
                        row.role.as_str(),
                        row.parent_instance_id,
                        row.worker_id,
                        row.job_id,
                        owner_json,
                        row.workspace.encoding,
                        row.workspace.bytes,
                        row.repo_id,
                        row.worktree_id,
                        row.branch_ref,
                        row.activity.as_str(),
                        row.activity_since_ms,
                        row.started_ms,
                        row.heartbeat_ms,
                        row.last_git_observed_ms,
                        row.lifecycle.as_str(),
                    ],
                )
                .map_err(|error| match error {
                    rusqlite::Error::SqliteFailure(code, _)
                        if code.code == rusqlite::ErrorCode::ConstraintViolation =>
                    {
                        invalid("instance_id already exists")
                    }
                    other => CoordinationError::from(other),
                })?;
            Ok(())
        })
    }

    /// Row snapshot capped at 101 so a 100-row view can report truncation.
    pub fn rows(&self, limit: usize) -> CoordResult<Vec<Registration>> {
        self.select_rows(limit, false)
    }

    /// Open rows only, filtered before the row cap.
    ///
    /// `Store::rows` returns every lifecycle state, so applying the cap there
    /// first could hide a newer open row behind terminal ones. The extra row is
    /// the truncation lookahead for a 100-row view.
    pub(super) fn open_rows(&self, limit: usize) -> CoordResult<Vec<Registration>> {
        self.select_rows(limit, true)
    }

    /// One ordered, capped SELECT; `open_only` applies the lifecycle filter
    /// before the limit so terminal rows cannot consume the view.
    fn select_rows(&self, limit: usize, open_only: bool) -> CoordResult<Vec<Registration>> {
        let mut statement = self.connection.prepare(
            "SELECT instance_id, data_namespace_id, session_id, kind, role,
                    parent_instance_id, worker_id, job_id, owner_json,
                    workspace_encoding, workspace_path, repo_id, worktree_id,
                    branch_ref, activity, activity_since_ms, started_ms,
                    heartbeat_ms, last_git_observed_ms, lifecycle
             FROM instances WHERE (?2 = 0 OR lifecycle='open')
             ORDER BY started_ms, instance_id LIMIT ?1",
        )?;
        let mut rows = statement.query(rusqlite::params![
            (limit.min(MAX_ROWS) + 1) as i64,
            open_only
        ])?;
        let mut collected = Vec::new();
        while let Some(row) = rows.next()? {
            collected.push(decode_row(row)?);
        }
        Ok(collected)
    }

    /// Generation-fenced heartbeat of an open row. Never resurrects.
    pub(crate) fn heartbeat(
        &mut self,
        id: &str,
        generation: &str,
        now_ms: i64,
        activity: Activity,
    ) -> CoordResult<bool> {
        self.heartbeat_with_workspace(id, generation, now_ms, activity, None)
    }

    /// One due tick: persist the heartbeat and, when a refresh succeeded, its
    /// workspace in the same immediate transaction.
    ///
    /// A refresh never replaces the heartbeat: both are written together, so an
    /// active session stays visibly active whether or not Git answered.
    pub(super) fn heartbeat_with_workspace(
        &mut self,
        id: &str,
        generation: &str,
        now_ms: i64,
        activity: Activity,
        workspace: Option<&super::workspace::WorkspaceIdentity>,
    ) -> CoordResult<bool> {
        if !is_uuid(id) || !is_uuid(generation) || now_ms < 0 {
            return Err(invalid("invalid heartbeat identity or timestamp"));
        }
        if let Some(workspace) = workspace {
            workspace.validate()?;
        }
        let Some(owner_json) = open_owner_json(&self.connection, id)? else {
            return Ok(false);
        };
        if generation_of(&owner_json)? != generation {
            return Ok(false);
        }
        self.write(|transaction| {
            let changed = transaction.execute(
                "UPDATE instances SET heartbeat_ms=?3, activity=?4,
                    activity_since_ms=CASE WHEN activity=?4 THEN activity_since_ms ELSE ?3 END
                 WHERE instance_id=?1 AND lifecycle='open' AND owner_json=?2",
                rusqlite::params![id, owner_json, now_ms, activity.as_str()],
            )?;
            if changed == 0 {
                return Ok(false);
            }
            match workspace {
                // The same owner JSON and open-row predicates fence both writes.
                Some(workspace) => {
                    write_workspace(transaction, id, &owner_json, workspace).map(|_| true)
                }
                None => Ok(true),
            }
        })
    }

    /// Generation-fenced close of an open row. Never touches another owner.

    pub(crate) fn close(&mut self, id: &str, generation: &str, now_ms: i64) -> CoordResult<bool> {
        if !is_uuid(id) || !is_uuid(generation) || now_ms < 0 {
            return Err(invalid("invalid close identity or timestamp"));
        }
        let Some(owner_json) = open_owner_json(&self.connection, id)? else {
            return Ok(false);
        };
        if generation_of(&owner_json)? != generation {
            return Ok(false);
        }
        self.write(|transaction| {
            let changed = transaction.execute(
                "UPDATE instances SET lifecycle='closed', heartbeat_ms=?3
                 WHERE instance_id=?1 AND lifecycle='open' AND owner_json=?2",
                rusqlite::params![id, owner_json, now_ms],
            )?;
            Ok(changed > 0)
        })
    }
    /// Generation-fenced workspace refresh of an open row. Never resurrects.

    pub(super) fn update_workspace(
        &mut self,
        id: &str,
        generation: &str,
        workspace: &super::workspace::WorkspaceIdentity,
        now_ms: i64,
    ) -> CoordResult<bool> {
        if !is_uuid(id) || !is_uuid(generation) || now_ms < 0 {
            return Err(invalid("invalid workspace identity or timestamp"));
        }
        // Validated before any write, so unpersistable metadata is rejected
        // instead of poisoning the whole snapshot.
        workspace.validate()?;
        let Some(owner_json) = open_owner_json(&self.connection, id)? else {
            return Ok(false);
        };
        if generation_of(&owner_json)? != generation {
            return Ok(false);
        }
        self.write(|transaction| {
            write_workspace(transaction, id, &owner_json, workspace)
        })
    }
}

/// The fenced workspace UPDATE. The open lifecycle and the exact owner JSON
/// re-check the generation that was validated before the transaction opened.
fn write_workspace(
    transaction: &rusqlite::Transaction<'_>,
    id: &str,
    owner_json: &str,
    workspace: &super::workspace::WorkspaceIdentity,
) -> CoordResult<bool> {
    let changed = transaction.execute(
        "UPDATE instances SET workspace_encoding=?3, workspace_path=?4,
            repo_id=?5, worktree_id=?6, branch_ref=?7, last_git_observed_ms=?8
         WHERE instance_id=?1 AND lifecycle='open' AND owner_json=?2",
        rusqlite::params![
            id,
            owner_json,
            workspace.workspace.encoding,
            workspace.workspace.bytes,
            workspace.repo_id,
            workspace.worktree_id,
            workspace.branch_ref,
            // Only a Git observation carries a Git time.
            workspace.repo_id.is_some().then_some(workspace.observed_ms),
        ],
    )?;
    Ok(changed > 0)
}

fn open_owner_json(connection: &Connection, id: &str) -> CoordResult<Option<String>> {
    connection
        .query_row(
            "SELECT owner_json FROM instances WHERE instance_id=?1 AND lifecycle='open'",
            [id],
            |row| row.get(0),
        )
        .optional()
        .map_err(CoordinationError::from)
}

fn generation_of(owner_json: &str) -> CoordResult<String> {
    let owner: OwnerRecord =
        serde_json::from_str(owner_json).map_err(|_| CoordinationError::Corrupt)?;
    Ok(owner.generation)
}

fn decode_row(row: &rusqlite::Row<'_>) -> CoordResult<Registration> {
    let text = |index: usize| -> CoordResult<String> { Ok(row.get(index)?) };
    let kind = ParticipantKind::from_text(&text(3)?).ok_or(CoordinationError::Corrupt)?;
    let role = ParticipantRole::from_text(&text(4)?).ok_or(CoordinationError::Corrupt)?;
    let activity = Activity::from_text(&text(14)?).ok_or(CoordinationError::Corrupt)?;
    let lifecycle = Lifecycle::from_text(&text(19)?).ok_or(CoordinationError::Corrupt)?;
    let owner: OwnerRecord =
        serde_json::from_str(&text(8)?).map_err(|_| CoordinationError::Corrupt)?;
    let workspace = NativePath {
        encoding: text(9)?,
        bytes: row.get(10)?,
    };
    let row = Registration {
        instance_id: text(0)?,
        data_namespace_id: text(1)?,
        session_id: row.get(2)?,
        kind,
        role,
        parent_instance_id: row.get(5)?,
        worker_id: row.get(6)?,
        job_id: row.get(7)?,
        owner,
        workspace,
        repo_id: row.get(11)?,
        worktree_id: row.get(12)?,
        branch_ref: row.get(13)?,
        activity,
        activity_since_ms: row.get(15)?,
        started_ms: row.get(16)?,
        heartbeat_ms: row.get(17)?,
        last_git_observed_ms: row.get(18)?,
        lifecycle,
    };
    // Persisted values must satisfy the same rules as inserted records; the
    // rejected value is never echoed.
    row.validate().map_err(|_| CoordinationError::Corrupt)?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths_in(dir: &Path) -> CoordinationPaths {
        CoordinationPaths {
            root: dir.join("coord"),
        }
    }

    fn hex64(fill: char) -> String {
        std::iter::repeat(fill).take(64).collect()
    }

    /// Test-only raw handle that retains the WAL pair like the product does.
    fn raw_connection(path: &Path) -> Connection {
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .unwrap();
        persist_wal(&connection).unwrap();
        connection
    }

    /// A comparable snapshot of one root: name, mode, inode and device.
    ///
    /// Contents are compared separately for the files whose bytes must not
    /// change; SQLite legitimately updates the SHM coordination file.
    #[cfg(unix)]
    fn snapshot(root: &Path) -> Vec<(String, u32, u64, u64)> {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let mut entries: Vec<_> = std::fs::read_dir(root)
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                let name = entry.file_name().to_string_lossy().into_owned();
                let metadata = std::fs::symlink_metadata(entry.path()).unwrap();
                (
                    name,
                    metadata.permissions().mode() & 0o7777,
                    metadata.ino(),
                    metadata.dev(),
                )
            })
            .collect();
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        entries
    }

    /// Names only, sorted, for entry-set comparisons.
    #[cfg(unix)]
    fn names(root: &Path) -> Vec<String> {
        snapshot(root).into_iter().map(|entry| entry.0).collect()
    }

    const STATE_FILES: [&str; 5] = [
        "coordination.sqlite3",
        "coordination.sqlite3-shm",
        "coordination.sqlite3-wal",
        "domain-id",
        "schema.lock",
    ];

    #[cfg(unix)]
    fn assert_private_state_files(root: &Path) {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        assert_eq!(names(root), STATE_FILES.map(String::from).to_vec());
        for name in STATE_FILES {
            let metadata = std::fs::symlink_metadata(root.join(name)).unwrap();
            assert!(metadata.is_file(), "{name} must be a regular file");
            assert_eq!(metadata.permissions().mode() & 0o7777, 0o600, "{name} mode");
            assert_eq!(metadata.uid(), unsafe { libc::geteuid() }, "{name} owner");
        }
    }

    fn bytes_of(root: &Path, name: &str) -> Vec<u8> {
        std::fs::read(root.join(name)).unwrap()
    }

    /// A valid idle store with one row, used as a repair fixture base.
    #[cfg(unix)]
    fn initialized_root(base: &Path, name: &str) -> CoordResult<CoordinationPaths> {
        let paths = paths_in(&base.join(name));
        let mut store = Store::open_rw(&paths)?;
        store.insert(&sample_row(
            &uuid::Uuid::new_v4().to_string(),
            &hex64('a'),
            None,
            1_000,
        ))?;
        drop(store);
        Ok(paths)
    }

    #[cfg(unix)]
    fn make_fifo(root: &Path, name: &str) {
        use std::{ffi::CString, os::unix::ffi::OsStrExt};

        let path = root.join(name);
        let c_path = CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: c_path remains alive and NUL-terminated throughout the call.
        let result = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        assert_eq!(
            result,
            0,
            "creating fixture FIFO {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        );
    }

    #[cfg(unix)]
    fn spawn_store_helper(mode: &str, fixture: &Path) -> std::process::Child {
        spawn_store_helper_with(mode, fixture, &[])
    }

    /// Start the ignored helper with extra child-only environment entries.
    #[cfg(unix)]
    fn spawn_store_helper_with(
        mode: &str,
        fixture: &Path,
        extra: &[(&str, &str)],
    ) -> std::process::Child {
        let nonce = format!("{}-{mode}", std::process::id());
        std::fs::write(fixture.join("nonce"), nonce.as_bytes()).unwrap();
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .arg("--exact")
            .arg("services::local_coordination::store::tests::lc_store_repair_helper")
            .arg("--ignored")
            .arg("--nocapture")
            .arg("--test-threads=1")
            .env("DCL_LC_STORE_MODE", mode)
            .env("DCL_LC_STORE_DIR", fixture)
            .env("DCL_LC_STORE_NONCE", nonce)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        for (key, value) in extra {
            command.env(key, value);
        }
        command.spawn().unwrap()
    }

    #[cfg(unix)]
    fn finish_store_helper(mut child: std::process::Child, seconds: u64) -> std::process::Output {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        loop {
            if child.try_wait().unwrap().is_some() {
                return child.wait_with_output().unwrap();
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let output = child.wait_with_output().unwrap();
                panic!(
                    "helper timed out: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Ignored child used for fixtures that must not touch the parent process:
    /// a hot rollback journal (forces `_exit` without SQLite cleanup) and a
    /// marker FIFO that would block a mutant which opens before validating.
    #[cfg(unix)]
    #[test]
    #[ignore = "process helper: started only by the registry process fixtures"]
    fn lc_store_repair_helper() {
        let mode = std::env::var("DCL_LC_STORE_MODE").expect("helper mode");
        let fixture = std::path::PathBuf::from(
            std::env::var_os("DCL_LC_STORE_DIR").expect("helper directory"),
        );
        let nonce = std::env::var("DCL_LC_STORE_NONCE").expect("helper nonce");
        let recorded = std::fs::read_to_string(fixture.join("nonce")).expect("fixture nonce");
        assert_eq!(recorded, nonce, "unexpected fixture nonce");
        let root = fixture.join("coord");
        let paths = CoordinationPaths { root: root.clone() };
        match mode.as_str() {
            "hot-journal" => {
                let connection = Connection::open(root.join("coordination.sqlite3")).unwrap();
                connection
                    .execute_batch(
                        "PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL;
                         PRAGMA cache_size=2; PRAGMA cache_spill=ON;
                         BEGIN IMMEDIATE;",
                    )
                    .unwrap();
                let mut statement = connection.prepare("UPDATE payload SET body=?1").unwrap();
                let body = vec![7u8; 3_500];
                for _ in 0..30 {
                    statement.execute([&body]).unwrap();
                }
                drop(statement);
                // Deliberately skip SQLite cleanup, leaving a hot journal.
                std::process::exit(0);
            }
            "fifo-marker" => {
                let read = Store::open_read(&paths);
                let write = Store::open_rw(&paths);
                assert!(
                    matches!(read, Err(CoordinationError::Corrupt)),
                    "reader accepted a FIFO marker"
                );
                assert!(
                    matches!(write, Err(CoordinationError::Corrupt)),
                    "writer accepted a FIFO marker"
                );
                std::fs::write(fixture.join("result"), b"corrupt").unwrap();
            }
            #[cfg(target_os = "linux")]
            "wal-write-lock" => {
                observe_wal_write_lock(&fixture, &nonce);
            }
            "snapshot-sqlite-bytes" => {
                // Copies SQLite-managed files without opening SQLite here, so
                // this PID holds no connection that could own their locks.
                for name in [DATABASE_NAME, WAL_NAME] {
                    let bytes = std::fs::read(root.join(name)).unwrap_or_default();
                    std::fs::write(fixture.join(format!("snapshot-{name}")), bytes).unwrap();
                }
                std::fs::write(fixture.join("snapshot-done"), &nonce).unwrap();
            }
            other => panic!("unknown helper mode {other}"),
        }
    }

    #[cfg(unix)]
    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// Query the WAL write-lock byte from an observer PID with no SQLite
    /// connection to that inode. `F_GETLK` only reads the kernel's table.
    #[cfg(target_os = "linux")]
    fn observe_wal_write_lock(fixture: &Path, nonce: &str) {
        use std::os::fd::AsRawFd;
        let shm = std::fs::File::open(fixture.join("coord").join(SHM_NAME)).expect("shm file");
        let mut lock: libc::flock = unsafe { std::mem::zeroed() };
        lock.l_type = libc::F_WRLCK as libc::c_short;
        lock.l_whence = libc::SEEK_SET as libc::c_short;
        // Byte 120 is the bundled SQLite 3.45 Unix WAL write-lock byte.
        lock.l_start = 120;
        lock.l_len = 1;
        let rc = unsafe { libc::fcntl(shm.as_raw_fd(), libc::F_GETLK, &mut lock) };
        assert_eq!(rc, 0, "F_GETLK failed");
        let kind = match lock.l_type as i32 {
            libc::F_UNLCK => "unlocked",
            libc::F_WRLCK => "write",
            libc::F_RDLCK => "read",
            other => panic!("unexpected lock type {other}"),
        };
        let label = std::env::var("DCL_LC_STORE_OBSERVATION").expect("observation label");
        std::fs::write(
            fixture.join(format!("lock-{label}")),
            format!("{nonce}\n{kind} {}\n", lock.l_pid),
        )
        .unwrap();
    }

    /// Read one observation: `(lock kind, owner pid)`.
    #[cfg(target_os = "linux")]
    fn read_observation(fixture: &Path, label: &str, nonce: &str) -> (String, libc::pid_t) {
        let path = fixture.join(format!("lock-{label}"));
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some(nonce), "stale observation in {label}");
        let line = lines.next().unwrap_or_default();
        let mut parts = line.split_whitespace();
        let kind = parts.next().unwrap_or("missing").to_string();
        let pid = parts
            .next()
            .and_then(|value| value.parse().ok())
            .unwrap_or(-1);
        (kind, pid)
    }

    /// Run the observer child once and decode its fresh observation.
    #[cfg(target_os = "linux")]
    fn observe_lock(fixture: &Path, label: &str) -> (String, libc::pid_t) {
        // A fresh label keeps stale result bytes from passing.
        let _ = std::fs::remove_file(fixture.join(format!("lock-{label}")));
        let child = spawn_store_helper_with(
            "wal-write-lock",
            fixture,
            &[("DCL_LC_STORE_OBSERVATION", label)],
        );
        let output = finish_store_helper(child, 60);
        assert!(
            output.status.success(),
            "lock observer failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let nonce = format!("{}-wal-write-lock", std::process::id());
        read_observation(fixture, label, &nonce)
    }

    #[cfg(unix)]
    #[test]
    fn local_coordination_missing_or_unsafe_wal_pair_is_preserved() -> CoordResult<()> {
        let temp = tempfile::tempdir().unwrap();

        // One sidecar removed, then both removed.
        for (index, removed) in [vec![WAL_NAME], vec![WAL_NAME, SHM_NAME]]
            .into_iter()
            .enumerate()
        {
            let paths = initialized_root(temp.path(), &format!("missing-{index}"))?;
            for name in &removed {
                std::fs::remove_file(paths.root.join(name)).unwrap();
            }
            let before = snapshot(&paths.root);
            let main = bytes_of(&paths.root, "coordination.sqlite3");
            let marker = bytes_of(&paths.root, "domain-id");
            assert!(matches!(
                Store::open_read(&paths),
                Err(CoordinationError::Corrupt)
            ));
            assert!(matches!(
                Store::open_rw(&paths),
                Err(CoordinationError::Corrupt)
            ));
            assert_eq!(snapshot(&paths.root), before, "no entries may be created");
            assert_eq!(bytes_of(&paths.root, "coordination.sqlite3"), main);
            assert_eq!(bytes_of(&paths.root, "domain-id"), marker);
        }

        // Each sidecar replaced by a symlink, a FIFO or a too-open file.
        for unsafe_kind in ["symlink", "fifo", "mode"] {
            let paths = initialized_root(temp.path(), unsafe_kind)?;
            let target = temp.path().join(format!("{unsafe_kind}-target"));
            std::fs::write(&target, b"outside-target").unwrap();
            set_mode(&target, 0o600);
            let wal = paths.root.join(WAL_NAME);
            std::fs::remove_file(&wal).unwrap();
            match unsafe_kind {
                "symlink" => std::os::unix::fs::symlink(&target, &wal).unwrap(),
                "fifo" => make_fifo(&paths.root, WAL_NAME),
                _ => {
                    std::fs::write(&wal, b"not really a wal").unwrap();
                    set_mode(&wal, 0o644);
                }
            }
            let before = snapshot(&paths.root);
            let main = bytes_of(&paths.root, "coordination.sqlite3");
            assert!(matches!(
                Store::open_read(&paths),
                Err(CoordinationError::Corrupt)
            ));
            assert!(matches!(
                Store::open_rw(&paths),
                Err(CoordinationError::Corrupt)
            ));
            assert_eq!(snapshot(&paths.root), before, "{unsafe_kind} entry set");
            assert_eq!(bytes_of(&paths.root, "coordination.sqlite3"), main);
            assert_eq!(std::fs::read(&target).unwrap(), b"outside-target");
            assert_eq!(
                std::fs::symlink_metadata(&target).unwrap().len(),
                "outside-target".len() as u64
            );
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn local_coordination_rejects_symlink_fifo_and_open_mode_markers() -> CoordResult<()> {
        let temp = tempfile::tempdir().unwrap();

        // A symlink to a matching 0600 marker outside the root.
        let paths = initialized_root(temp.path(), "marker-symlink")?;
        let domain = bytes_of(&paths.root, DOMAIN_MARKER);
        let target = temp.path().join("outside-marker");
        std::fs::write(&target, &domain).unwrap();
        set_mode(&target, 0o600);
        std::fs::remove_file(paths.root.join(DOMAIN_MARKER)).unwrap();
        std::os::unix::fs::symlink(&target, paths.root.join(DOMAIN_MARKER)).unwrap();
        let before = snapshot(&paths.root);
        let main = bytes_of(&paths.root, "coordination.sqlite3");
        assert!(matches!(
            Store::open_read(&paths),
            Err(CoordinationError::Corrupt)
        ));
        assert!(matches!(
            Store::open_rw(&paths),
            Err(CoordinationError::Corrupt)
        ));
        assert_eq!(snapshot(&paths.root), before);
        assert_eq!(bytes_of(&paths.root, "coordination.sqlite3"), main);
        assert_eq!(std::fs::read(&target).unwrap(), domain, "target bytes");

        // A matching marker regular file with too-open permissions.
        let paths = initialized_root(temp.path(), "marker-mode")?;
        set_mode(&paths.root.join(DOMAIN_MARKER), 0o644);
        let before = snapshot(&paths.root);
        let marker = bytes_of(&paths.root, DOMAIN_MARKER);
        assert!(matches!(
            Store::open_read(&paths),
            Err(CoordinationError::Corrupt)
        ));
        assert!(matches!(
            Store::open_rw(&paths),
            Err(CoordinationError::Corrupt)
        ));
        assert_eq!(snapshot(&paths.root), before);
        assert_eq!(bytes_of(&paths.root, DOMAIN_MARKER), marker);

        // A FIFO marker must not block: the bounded child proves both opens
        // reject it (and any mutant that opens before validating fails by
        // the deadline instead of hanging the suite).
        let fixture = temp.path().join("marker-fifo");
        std::fs::create_dir_all(&fixture)?;
        let paths = initialized_root(&fixture, "coord")?;
        std::fs::remove_file(paths.root.join(DOMAIN_MARKER)).unwrap();
        make_fifo(&paths.root, DOMAIN_MARKER);
        let before = snapshot(&paths.root);
        let child = spawn_store_helper("fifo-marker", &fixture);
        let output = finish_store_helper(child, 60);
        assert!(
            output.status.success(),
            "helper failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(std::fs::read(fixture.join("result")).unwrap(), b"corrupt");
        assert_eq!(snapshot(&paths.root), before, "FIFO marker preserved");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn local_coordination_orphan_sidecars_are_errors_without_new_lock() -> CoordResult<()> {
        let temp = tempfile::tempdir().unwrap();

        let absent = paths_in(&temp.path().join("absent"));
        assert!(Store::open_read(&absent)?.is_none());
        assert!(!absent.root.exists());

        let empty = paths_in(&temp.path().join("empty"));
        std::fs::create_dir_all(&empty.root)?;
        set_mode(&empty.root, 0o700);
        assert!(Store::open_read(&empty)?.is_none());
        assert!(std::fs::read_dir(&empty.root)?.next().is_none());

        let lock_only = paths_in(&temp.path().join("lock-only"));
        std::fs::create_dir_all(&lock_only.root)?;
        set_mode(&lock_only.root, 0o700);
        std::fs::write(lock_only.root.join(SCHEMA_LOCK), b"").unwrap();
        set_mode(&lock_only.root.join(SCHEMA_LOCK), 0o600);
        assert!(Store::open_read(&lock_only)?.is_none());
        assert_eq!(names(&lock_only.root), vec![String::from(SCHEMA_LOCK)]);

        for (index, orphan) in [
            vec![WAL_NAME],
            vec![SHM_NAME],
            vec![WAL_NAME, SHM_NAME],
            vec!["unrelated-file"],
        ]
        .into_iter()
        .enumerate()
        {
            let paths = paths_in(&temp.path().join(format!("orphan-{index}")));
            std::fs::create_dir_all(&paths.root)?;
            set_mode(&paths.root, 0o700);
            for name in &orphan {
                std::fs::write(paths.root.join(name), b"orphan").unwrap();
                set_mode(&paths.root.join(name), 0o600);
            }
            let before = snapshot(&paths.root);
            assert!(matches!(
                Store::open_read(&paths),
                Err(CoordinationError::Corrupt)
            ));
            assert!(matches!(
                Store::open_rw(&paths),
                Err(CoordinationError::Corrupt)
            ));
            assert_eq!(snapshot(&paths.root), before, "orphans preserved");
        }

        // A database without marker or lock must not gain a lock.
        let db_only = paths_in(&temp.path().join("db-only"));
        std::fs::create_dir_all(&db_only.root)?;
        set_mode(&db_only.root, 0o700);
        std::fs::write(db_only.root.join(DATABASE_NAME), [0u8; 100]).unwrap();
        set_mode(&db_only.root.join(DATABASE_NAME), 0o600);
        let before = snapshot(&db_only.root);
        assert!(matches!(
            Store::open_rw(&db_only),
            Err(CoordinationError::Corrupt | CoordinationError::DomainMismatch)
        ));
        assert!(matches!(
            Store::open_read(&db_only),
            Err(CoordinationError::DomainMismatch)
        ));
        assert_eq!(snapshot(&db_only.root), before);
        assert!(!db_only.root.join(SCHEMA_LOCK).exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn local_coordination_write_rechecks_schema_after_immediate_begin() -> CoordResult<()> {
        use std::cell::Cell;
        let temp = tempfile::tempdir().unwrap();
        let paths = paths_in(temp.path());
        let mut store = Store::open_rw(&paths)?;
        store.connection().busy_timeout(Duration::from_millis(25))?;

        // A peer holding the write reservation makes the immediate begin fail.
        let mut peer = raw_connection(&paths.database());
        let reservation = peer.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let ran = Cell::new(false);
        let outcome = store.write(|_| {
            ran.set(true);
            Ok(())
        });
        assert!(matches!(outcome, Err(CoordinationError::Busy)));
        assert!(!ran.get(), "a busy transaction must not run the closure");
        drop(reservation);

        // A schema change committed by the peer is caught by the in-transaction
        // recheck on the already-open store handle.
        peer.execute_batch("PRAGMA user_version=2")?;
        drop(peer);
        let outcome = store.write(|_| {
            ran.set(true);
            Ok(())
        });
        assert!(matches!(
            outcome,
            Err(CoordinationError::UnsupportedSchema(2))
        ));
        assert!(!ran.get(), "the schema fence must run before the closure");
        Ok(())
    }

    #[cfg(unix)]
    fn while_peer_holds_sqlite_lock<T>(
        paths: &CoordinationPaths,
        exclusive: bool,
        operation: impl FnOnce() -> CoordResult<T>,
    ) -> CoordResult<T> {
        use std::sync::mpsc;

        let database = paths.database();
        let peer_reservation = Duration::from_millis(350);
        let (ready, reserved) = mpsc::channel();
        let (start, attempting) = mpsc::channel();
        std::thread::scope(|scope| {
            let peer = scope.spawn(move || -> CoordResult<()> {
                let mut connection = raw_connection(&database);
                if exclusive {
                    connection.pragma_update(None, "locking_mode", "EXCLUSIVE")?;
                }
                let reservation =
                    connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
                ready.send(()).unwrap();
                attempting.recv_timeout(Duration::from_secs(10)).unwrap();
                std::thread::sleep(peer_reservation);
                reservation.commit()?;
                Ok(())
            });
            reserved.recv_timeout(Duration::from_secs(10)).unwrap();
            let started = Instant::now();
            start.send(()).unwrap();
            let result = operation();
            let elapsed = started.elapsed();
            peer.join().unwrap()?;
            eprintln!(
                "SQLite operation took {elapsed:?}, result {:?}",
                result.as_ref().map(|_| ())
            );
            let result = result?;
            assert!(
                elapsed >= peer_reservation,
                "operation bypassed the peer lock"
            );
            Ok(result)
        })
    }

    #[cfg(unix)]
    #[test]
    fn local_coordination_write_waits_for_peer_commit() -> CoordResult<()> {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths_in(temp.path());
        let mut store = Store::open_rw(&paths)?;
        while_peer_holds_sqlite_lock(&paths, false, || {
            store.insert(&sample_row(
                &uuid::Uuid::new_v4().to_string(),
                &hex64('a'),
                None,
                1_000,
            ))
        })?;
        let read = Store::open_read(&paths)?.expect("initialized registry");
        assert_eq!(read.rows(10)?.len(), 1);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn local_coordination_opens_wait_for_peer_lock() -> CoordResult<()> {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths_in(temp.path());
        let domain = Store::open_rw(&paths)?.domain_id().to_string();

        // WAL readers coexist with a normal writer. An exclusive peer lock
        // forces each open to wait during SQLite schema validation.
        let read = while_peer_holds_sqlite_lock(&paths, true, || Store::open_read(&paths))?
            .expect("initialized registry");
        assert_eq!(read.domain_id(), domain);
        drop(read);
        let write = while_peer_holds_sqlite_lock(&paths, true, || Store::open_rw(&paths))?;
        assert_eq!(write.domain_id(), domain);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn local_coordination_rows_are_sorted_and_capped_at_101() -> CoordResult<()> {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths_in(temp.path());
        let mut store = Store::open_rw(&paths)?;
        let mut expected: Vec<(i64, String)> = (0..110u128)
            .map(|index| {
                (
                    // Repeated and interleaved timestamps.
                    ((index % 7) as i64) * 1_000,
                    uuid::Uuid::from_u128(0x1000 + index).to_string(),
                )
            })
            .collect();
        expected.sort();
        // Opposite of the query order, so ORDER BY actually decides.
        for (started, id) in expected.iter().rev() {
            store.insert(&sample_row(id, &hex64('a'), None, *started))?;
        }

        let want: Vec<String> = expected
            .iter()
            .take(101)
            .map(|(_, id)| id.clone())
            .collect();
        let collect = |limit: usize| -> CoordResult<Vec<String>> {
            Ok(store
                .rows(limit)?
                .into_iter()
                .map(|row| row.instance_id)
                .collect())
        };
        assert_eq!(collect(usize::MAX)?, want, "capped at 101 rows");
        assert_eq!(collect(100)?, want, "100-row view keeps one lookahead row");
        assert_eq!(collect(1)?, want[..2].to_vec(), "lookahead for truncation");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn local_coordination_native_path_validation_fences_insert_and_decode() -> CoordResult<()> {
        use std::os::unix::ffi::OsStrExt;
        let temp = tempfile::tempdir().unwrap();
        let paths = paths_in(temp.path().join("fence").as_path());
        let mut store = Store::open_rw(&paths)?;
        let valid = sample_row(&uuid::Uuid::new_v4().to_string(), &hex64('a'), None, 1_000);
        store.insert(&valid)?;

        for bad in [
            NativePath {
                encoding: "windows_utf16le".into(),
                bytes: b"/x".to_vec(),
            },
            NativePath {
                encoding: "unix_bytes".into(),
                bytes: Vec::new(),
            },
            NativePath {
                encoding: "unix_bytes".into(),
                bytes: vec![b'a'; 8_193],
            },
            NativePath {
                encoding: "unix_bytes".into(),
                bytes: b"/tmp/a\0b".to_vec(),
            },
        ] {
            let mut row = valid.clone();
            row.instance_id = uuid::Uuid::new_v4().to_string();
            row.workspace = bad;
            assert!(matches!(
                store.insert(&row),
                Err(CoordinationError::InvalidRecord(_))
            ));
            assert_eq!(store.rows(10)?, vec![valid.clone()], "row set unchanged");
        }
        let foreign = NativePath {
            encoding: "plan9_bytes".into(),
            bytes: b"/x".to_vec(),
        };
        assert!(matches!(
            foreign.to_path(),
            Err(CoordinationError::InvalidRecord(_))
        ));

        // A valid non-UTF-8 path roundtrips through SQL and native decoding.
        let mut row = valid.clone();
        row.instance_id = uuid::Uuid::new_v4().to_string();
        row.workspace = NativePath {
            encoding: "unix_bytes".into(),
            bytes: b"/tmp/\xff-ws".to_vec(),
        };
        store.insert(&row)?;
        assert_eq!(store.rows(10)?.len(), 2);
        assert_eq!(
            row.workspace.to_path()?,
            std::path::PathBuf::from(std::ffi::OsStr::from_bytes(b"/tmp/\xff-ws"))
        );
        drop(store);

        // Corrupted persisted values fail at the decode boundary and the
        // fixture is restored between mutations.
        let decode = initialized_root(temp.path(), "decode")?;
        let instance = {
            let reader = Store::open_read(&decode)?.expect("store exists");
            reader.rows(10)?[0].instance_id.clone()
        };
        let mutations = [
            (
                "UPDATE instances SET workspace_encoding='windows_utf16le', workspace_path=x'2f78'"
                    .to_string(),
                "UPDATE instances SET workspace_encoding='unix_bytes', workspace_path=x'2f746d70'"
                    .to_string(),
            ),
            (
                "UPDATE instances SET workspace_path=x'2f0061'".to_string(),
                "UPDATE instances SET workspace_path=x'2f746d70'".to_string(),
            ),
            (
                "UPDATE instances SET workspace_path=x''".to_string(),
                "UPDATE instances SET workspace_path=x'2f746d70'".to_string(),
            ),
            (
                "UPDATE instances SET instance_id='not-a-uuid'".to_string(),
                format!(
                    "UPDATE instances SET instance_id='{instance}' WHERE instance_id='not-a-uuid'"
                ),
            ),
            (
                "UPDATE instances SET started_ms=-5".to_string(),
                "UPDATE instances SET started_ms=1000".to_string(),
            ),
        ];
        for (mutation, restore) in mutations {
            let raw = raw_connection(&decode.database());
            raw.execute_batch(&mutation)?;
            drop(raw);
            let reader = Store::open_read(&decode)?.expect("store exists");
            assert!(
                matches!(reader.rows(10), Err(CoordinationError::Corrupt)),
                "accepted a malformed persisted value: {mutation}"
            );
            drop(reader);
            let raw = raw_connection(&decode.database());
            raw.execute_batch(&restore)?;
            drop(raw);
            let reader = Store::open_read(&decode)?.expect("store exists");
            assert_eq!(reader.rows(10)?.len(), 1, "restored after {mutation}");
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn local_coordination_sqlite_corruption_maps_to_corrupt() -> CoordResult<()> {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths_in(temp.path());
        drop(Store::open_rw(&paths)?);
        std::fs::write(paths.database(), b"garbage, not sqlite").unwrap();
        set_mode(&paths.database(), 0o600);
        let before = snapshot(&paths.root);
        let db_bytes = bytes_of(&paths.root, "coordination.sqlite3");
        assert!(matches!(
            Store::open_read(&paths),
            Err(CoordinationError::Corrupt)
        ));
        assert!(matches!(
            Store::open_rw(&paths),
            Err(CoordinationError::Corrupt)
        ));
        assert_eq!(snapshot(&paths.root), before);
        assert_eq!(bytes_of(&paths.root, "coordination.sqlite3"), db_bytes);

        // The mapping itself is asserted, so it stays covered even though the
        // file-header preflight usually rejects these fixtures first.
        for (code, expected) in [
            (rusqlite::ffi::SQLITE_NOTADB, "corrupt"),
            (rusqlite::ffi::SQLITE_CORRUPT, "corrupt"),
            (rusqlite::ffi::SQLITE_BUSY, "busy"),
            (rusqlite::ffi::SQLITE_LOCKED, "busy"),
        ] {
            let mapped = CoordinationError::from(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(code),
                None,
            ));
            match expected {
                "corrupt" => assert!(matches!(mapped, CoordinationError::Corrupt), "{code}"),
                _ => assert!(matches!(mapped, CoordinationError::Busy), "{code}"),
            }
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn local_coordination_rejects_hot_rollback_journal_without_recovery() -> CoordResult<()> {
        let temp = tempfile::tempdir().unwrap();
        let fixture = temp.path().join("hot");
        let root = fixture.join("coord");
        std::fs::create_dir_all(&root)?;
        set_mode(&fixture, 0o700);
        set_mode(&root, 0o700);
        std::fs::write(root.join(DOMAIN_MARKER), uuid::Uuid::new_v4().to_string()).unwrap();
        set_mode(&root.join(DOMAIN_MARKER), 0o600);
        let database = root.join(DATABASE_NAME);

        // A DELETE-mode fixture with committed payload rows.
        {
            let connection = Connection::open(&database)?;
            connection.execute_batch(
                "PRAGMA user_version=2;
                 CREATE TABLE payload(id INTEGER PRIMARY KEY, body BLOB NOT NULL);
                 BEGIN IMMEDIATE;",
            )?;
            let body = vec![1u8; 3_500];
            {
                let mut statement = connection.prepare("INSERT INTO payload(body) VALUES(?1)")?;
                for _ in 0..30 {
                    statement.execute([&body])?;
                }
            }
            connection.execute_batch("COMMIT")?;
        }
        set_mode(&database, 0o600);
        let committed = std::fs::read(&database)?;
        let marker = std::fs::read(root.join(DOMAIN_MARKER)).unwrap();

        let child = spawn_store_helper("hot-journal", &fixture);
        let output = finish_store_helper(child, 120);
        assert!(
            output.status.success(),
            "helper failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        // The child left a hot journal and uncommitted page changes behind.
        let journal = root.join(JOURNAL_NAME);
        let journal_bytes = std::fs::read(&journal)?;
        assert!(!journal_bytes.is_empty(), "a hot journal must exist");
        assert_ne!(
            &journal_bytes[0..8],
            &[0u8; 8],
            "the journal must carry its signature"
        );
        let hot = std::fs::read(&database)?;
        assert_ne!(hot, committed, "the fixture must hold uncommitted pages");

        let paths = CoordinationPaths { root: root.clone() };
        let before = snapshot(&root);
        assert!(matches!(
            Store::open_read(&paths),
            Err(CoordinationError::Corrupt)
        ));
        assert!(matches!(
            Store::open_rw(&paths),
            Err(CoordinationError::Corrupt)
        ));
        // Nothing was recovered, checkpointed, created or removed.
        assert_eq!(snapshot(&root), before);
        assert_eq!(std::fs::read(&database)?, hot);
        assert_eq!(std::fs::read(root.join(DOMAIN_MARKER))?, marker);
        assert_eq!(std::fs::read(&journal)?, journal_bytes);
        assert!(journal.exists(), "the journal must still be present");
        assert!(
            !root.join(SCHEMA_LOCK).exists(),
            "a rejected root must not gain a schema lock"
        );

        // Second phase: with a real private schema lock present, the writer
        // acquires it and still refuses the journal, preserving everything.
        {
            use std::io::Write as _;
            let mut lock = std::fs::File::create(root.join(SCHEMA_LOCK))?;
            lock.write_all(b"")?;
            lock.sync_all()?;
        }
        set_mode(&root.join(SCHEMA_LOCK), 0o600);
        let locked_before = snapshot(&root);
        assert!(locked_before.iter().any(|entry| entry.0 == SCHEMA_LOCK));
        assert!(matches!(
            Store::open_read(&paths),
            Err(CoordinationError::Corrupt)
        ));
        assert!(matches!(
            Store::open_rw(&paths),
            Err(CoordinationError::Corrupt)
        ));
        assert_eq!(snapshot(&root), locked_before);
        assert_eq!(std::fs::read(&database)?, hot);
        assert_eq!(std::fs::read(root.join(DOMAIN_MARKER))?, marker);
        assert_eq!(std::fs::read(&journal)?, journal_bytes);
        assert!(journal.exists(), "the journal must still be present");
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn local_coordination_public_store_open_preserves_peer_process_writer_lock() -> CoordResult<()>
    {
        let parent = std::process::id() as libc::pid_t;
        for kind in ["read", "write"] {
            let temp = tempfile::tempdir().unwrap();
            let fixture = temp.path().join("fixture");
            std::fs::create_dir_all(&fixture).unwrap();
            // The helper always looks for the store at `fixture/coord`.
            let paths = paths_in(&fixture);
            {
                let mut seed = Store::open_rw(&paths)?;
                seed.insert(&sample_row(
                    &uuid::Uuid::new_v4().to_string(),
                    &hex64('a'),
                    None,
                    1_000,
                ))?;
            }

            // First writer holds a real immediate reservation on the WAL.
            let mut writer = Store::open_rw(&paths)?;
            writer.connection().execute_batch("BEGIN IMMEDIATE")?;
            writer.connection().execute(
                "UPDATE instances SET activity='active' WHERE lifecycle='open'",
                [],
            )?;

            let before = observe_lock(&fixture, "before");
            assert_eq!(before.0, "write", "{kind}: reservation before open");
            assert_eq!(before.1, parent, "{kind}: reservation owner before open");

            // Opening a second public Store in this PID must not release it.
            let second = match kind {
                "read" => {
                    Some(Store::open_read(&paths)?.expect("an initialized store is readable"))
                }
                _ => Some(Store::open_rw(&paths)?),
            };

            let after = observe_lock(&fixture, "after");
            assert_eq!(
                after.0, "write",
                "{kind}: the peer reservation must survive the second open"
            );
            assert_eq!(after.1, parent, "{kind}: reservation owner after open");
            eprintln!("{kind}: before={before:?} after={after:?}");
            drop(second);

            writer.connection().execute_batch("ROLLBACK")?;
            drop(writer);
            let released = observe_lock(&fixture, "released");
            assert_eq!(released.0, "unlocked", "{kind}: locks release on close");
        }
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn local_coordination_read_idle_wal_keeps_entries() -> CoordResult<()> {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths_in(temp.path());
        let mut store = Store::open_rw(&paths)?;
        store.insert(&sample_row(
            &uuid::Uuid::new_v4().to_string(),
            &hex64('a'),
            None,
            1_000,
        ))?;
        drop(store);

        assert_private_state_files(&paths.root);
        let before = snapshot(&paths.root);
        let main = bytes_of(&paths.root, "coordination.sqlite3");
        let marker = bytes_of(&paths.root, "domain-id");
        let wal = bytes_of(&paths.root, "coordination.sqlite3-wal");

        let reader = Store::open_read(&paths)?.expect("an initialized store exists");
        assert_eq!(reader.rows(10)?.len(), 1);
        assert_eq!(
            snapshot(&paths.root),
            before,
            "the read must not touch files"
        );
        drop(reader);
        assert_eq!(snapshot(&paths.root), before, "entries survive the read");
        assert_eq!(bytes_of(&paths.root, "coordination.sqlite3"), main);
        assert_eq!(bytes_of(&paths.root, "domain-id"), marker);
        assert_eq!(bytes_of(&paths.root, "coordination.sqlite3-wal"), wal);
        Ok(())
    }

    /// Copy the SQLite-managed files from a child PID that holds no SQLite
    /// connection, so this process never opens them alongside its connections.
    #[cfg(unix)]
    fn snapshot_sqlite_bytes(fixture: &Path) -> CoordResult<(Vec<u8>, Vec<u8>)> {
        let _ = std::fs::remove_file(fixture.join("snapshot-done"));
        let child = spawn_store_helper("snapshot-sqlite-bytes", fixture);
        let output = finish_store_helper(child, 60);
        assert!(
            output.status.success(),
            "snapshot helper failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let nonce = format!("{}-snapshot-sqlite-bytes", std::process::id());
        let done = std::fs::read_to_string(fixture.join("snapshot-done"))
            .expect("snapshot helper must publish its marker");
        assert_eq!(done, nonce, "stale snapshot bytes");
        Ok((
            std::fs::read(fixture.join(format!("snapshot-{DATABASE_NAME}")))?,
            std::fs::read(fixture.join(format!("snapshot-{WAL_NAME}")))?,
        ))
    }

    #[cfg(unix)]
    #[test]
    fn local_coordination_read_live_wal_keeps_entries_and_sees_commit() -> CoordResult<()> {
        let temp = tempfile::tempdir().unwrap();
        let fixture = temp.path().join("fixture");
        std::fs::create_dir_all(&fixture)?;
        let paths = paths_in(&fixture);
        let mut writer = Store::open_rw(&paths)?;
        writer
            .connection()
            .execute_batch("PRAGMA wal_autocheckpoint=0")?;
        let (main_before, wal_before) = snapshot_sqlite_bytes(&fixture)?;
        let before = snapshot(&paths.root);
        let marker_before = bytes_of(&paths.root, "domain-id");

        writer.insert(&sample_row(
            &uuid::Uuid::new_v4().to_string(),
            &hex64('b'),
            None,
            2_000,
        ))?;
        // The committed row lives only in the WAL: main is unchanged, WAL grew.
        let (main_after, wal_after) = snapshot_sqlite_bytes(&fixture)?;
        assert_eq!(
            main_after, main_before,
            "the row must not be checkpointed into the main file"
        );
        assert_ne!(wal_after, wal_before, "the WAL must carry the new commit");

        let reader = Store::open_read(&paths)?.expect("an initialized store exists");
        let rows = reader.rows(10)?;
        assert_eq!(rows.len(), 1, "the reader must see the live commit");
        assert_eq!(rows[0].started_ms, 2_000);
        assert_eq!(
            snapshot(&paths.root),
            before,
            "the live read creates nothing"
        );
        drop(reader);
        assert_eq!(snapshot(&paths.root), before);
        let (main_final, wal_final) = snapshot_sqlite_bytes(&fixture)?;
        assert_eq!(main_final, main_before);
        assert_eq!(wal_final, wal_after, "the final WAL must keep the commit");
        assert_eq!(bytes_of(&paths.root, "domain-id"), marker_before);
        assert_eq!(writer.rows(10)?.len(), 1);
        Ok(())
    }

    fn sample_row(
        instance: &str,
        namespace: &str,
        session: Option<&str>,
        started_ms: i64,
    ) -> Registration {
        Registration {
            instance_id: instance.into(),
            data_namespace_id: namespace.into(),
            session_id: session.map(Into::into),
            kind: ParticipantKind::Cli,
            role: ParticipantRole::Frontend,
            parent_instance_id: None,
            worker_id: None,
            job_id: None,
            owner: OwnerRecord {
                platform: "linux".into(),
                pid: std::process::id(),
                boot_id: uuid::Uuid::new_v4().to_string(),
                creation_id: "424242".into(),
                generation: uuid::Uuid::new_v4().to_string(),
            },
            workspace: NativePath::from_path(Path::new("/workspace/example")),
            repo_id: None,
            worktree_id: None,
            branch_ref: None,
            activity: Activity::Idle,
            activity_since_ms: started_ms,
            started_ms,
            heartbeat_ms: started_ms,
            last_git_observed_ms: None,
            lifecycle: Lifecycle::Open,
        }
    }

    #[cfg(unix)]
    #[test]
    fn local_coordination_registry_roundtrip() -> CoordResult<()> {
        let temp = tempfile::tempdir().unwrap();
        let paths = paths_in(temp.path());
        let mut store = Store::open_rw(&paths)?;

        let mut row = sample_row(
            &uuid::Uuid::new_v4().to_string(),
            &hex64('a'),
            Some(&uuid::Uuid::new_v4().to_string()),
            1_000,
        );
        row.kind = ParticipantKind::BackgroundAgent;
        row.role = ParticipantRole::Executor;
        row.parent_instance_id = Some(uuid::Uuid::new_v4().to_string());
        row.worker_id = Some("worker-1".into());
        row.job_id = Some("job-1".into());
        row.repo_id = Some(hex64('b'));
        row.worktree_id = Some(hex64('c'));
        row.branch_ref = Some(b"refs/heads/\xff-feature".to_vec());
        row.activity = Activity::Active;
        row.activity_since_ms = 500;
        row.heartbeat_ms = 1_200;
        row.last_git_observed_ms = Some(1_100);
        row.workspace = NativePath {
            encoding: "unix_bytes".into(),
            bytes: b"/tmp/\xff-path".to_vec(),
        };
        let generation = row.owner.generation.clone();
        let instance = row.instance_id.clone();

        store.insert(&row)?;
        assert_eq!(store.rows(10)?, vec![row.clone()]);
        assert!(matches!(
            store.insert(&row),
            Err(CoordinationError::InvalidRecord(_))
        ));
        drop(store);

        // Reopen and compare every field losslessly, including native bytes.
        let mut store = Store::open_rw(&paths)?;
        assert_eq!(store.rows(10)?, vec![row.clone()]);

        let wrong = uuid::Uuid::new_v4().to_string();
        assert!(!store.heartbeat(&instance, &wrong, 2_000, Activity::HeavyCommand)?);
        assert!(!store.close(&instance, &wrong, 2_000)?);
        assert_eq!(store.rows(10)?, vec![row.clone()]);

        assert!(store.heartbeat(&instance, &generation, 2_000, Activity::HeavyCommand)?);
        let after = store.rows(10)?.remove(0);
        assert_eq!(after.activity, Activity::HeavyCommand);
        assert_eq!(after.heartbeat_ms, 2_000);
        assert_eq!(after.activity_since_ms, 2_000);

        // A repeated activity keeps its original start time.
        assert!(store.heartbeat(&instance, &generation, 2_500, Activity::HeavyCommand)?);
        assert_eq!(store.rows(10)?[0].activity_since_ms, 2_000);

        assert!(store.close(&instance, &generation, 3_000)?);
        assert!(!store.close(&instance, &generation, 3_100)?);
        assert!(!store.heartbeat(&instance, &generation, 3_200, Activity::Active)?);
        let closed = store.rows(10)?.remove(0);
        assert_eq!(closed.lifecycle, Lifecycle::Closed);
        assert_eq!(closed.heartbeat_ms, 3_000);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn local_coordination_two_roots_isolate() -> CoordResult<()> {
        let temp = tempfile::tempdir().unwrap();
        let left = paths_in(&temp.path().join("left"));
        let right = paths_in(&temp.path().join("right"));
        let mut left_store = Store::open_rw(&left)?;
        let right_store = Store::open_rw(&right)?;
        assert_ne!(left_store.domain_id(), right_store.domain_id());
        let right_domain = right_store.domain_id().to_string();
        drop(right_store);

        let session = uuid::Uuid::new_v4().to_string();
        left_store.insert(&sample_row(
            &uuid::Uuid::new_v4().to_string(),
            &hex64('a'),
            Some(&session),
            1_000,
        ))?;
        assert_eq!(left_store.rows(10)?.len(), 1);

        let read = Store::open_read(&right)?.expect("right store exists");
        assert_eq!(read.domain_id(), right_domain);
        assert!(read.rows(10)?.is_empty());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn local_coordination_two_data_dirs_share_root() -> CoordResult<()> {
        let temp = tempfile::tempdir().unwrap();
        // Two unrelated data directories with different settings never split
        // or feed the coordination root.
        let data_a = temp.path().join("data-a");
        let data_b = temp.path().join("data-b");
        for (dir, enabled) in [(&data_a, "false"), (&data_b, "true")] {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(
                dir.join("settings.json"),
                format!(r#"{{"localCoordination":{{"enabled":{enabled}}}}}"#),
            )
            .unwrap();
        }
        let paths = paths_in(temp.path());
        let mut store = Store::open_rw(&paths)?;

        // The same session UUID in two namespaces stays separately addressable.
        let session = uuid::Uuid::new_v4().to_string();
        let first = sample_row(
            &uuid::Uuid::new_v4().to_string(),
            &hex64('a'),
            Some(&session),
            1_000,
        );
        let second = sample_row(
            &uuid::Uuid::new_v4().to_string(),
            &hex64('b'),
            Some(&session),
            1_000,
        );
        store.insert(&first)?;
        store.insert(&second)?;

        let rows = store.rows(10)?;
        assert_eq!(rows.len(), 2);
        for (namespace, expected) in [
            (&first.data_namespace_id, &first),
            (&second.data_namespace_id, &second),
        ] {
            let matching: Vec<_> = rows
                .iter()
                .filter(|row| {
                    &row.data_namespace_id == namespace
                        && row.session_id.as_deref() == Some(session.as_str())
                })
                .collect();
            assert_eq!(matching, vec![expected]);
        }
        // Neither data directory is used as store state.
        assert!(!paths.root.join("data-a").exists());
        assert!(!paths.root.join("data-b").exists());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn local_coordination_redaction_and_native_paths() -> CoordResult<()> {
        use base64::{engine::general_purpose::STANDARD, Engine};

        let temp = tempfile::tempdir().unwrap();
        let paths = paths_in(temp.path());
        let mut store = Store::open_rw(&paths)?;
        let mut row = sample_row(
            &uuid::Uuid::new_v4().to_string(),
            &hex64('a'),
            Some(&uuid::Uuid::new_v4().to_string()),
            1_000,
        );
        row.workspace = NativePath {
            encoding: "unix_bytes".into(),
            bytes: b"/space/\x01\xff dir".to_vec(),
        };
        row.branch_ref = Some(b"refs/heads/\xff\x1b-branch".to_vec());
        store.insert(&row)?;
        let stored = store.rows(10)?.remove(0);

        // Native bytes and non-UTF-8 refs stay byte-exact.
        assert_eq!(stored.workspace.bytes, b"/space/\x01\xff dir".to_vec());
        assert_eq!(stored.branch_ref, row.branch_ref);

        // Only allowlisted columns exist; there is no free-form metadata.
        let columns: Vec<String> = store
            .connection()
            .prepare("PRAGMA table_info(instances)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<_>>()?;
        let expected = [
            "instance_id",
            "data_namespace_id",
            "session_id",
            "kind",
            "role",
            "parent_instance_id",
            "worker_id",
            "job_id",
            "owner_json",
            "workspace_encoding",
            "workspace_path",
            "repo_id",
            "worktree_id",
            "branch_ref",
            "activity",
            "activity_since_ms",
            "started_ms",
            "heartbeat_ms",
            "last_git_observed_ms",
            "lifecycle",
        ];
        assert_eq!(columns, expected.map(String::from).to_vec());

        // JSON exports bytes as base64 and never exposes forbidden fields.
        let json = serde_json::to_string(&stored).unwrap();
        for forbidden in [
            "prompt",
            "title",
            "argv",
            "env",
            "command",
            "apiKey",
            "api_key",
            "environment",
            "settings",
        ] {
            assert!(
                !json.contains(&format!("\"{forbidden}\"")),
                "JSON exposed {forbidden}: {json}"
            );
        }
        assert!(json.contains(&STANDARD.encode(&stored.workspace.bytes)));
        assert!(
            !json.as_bytes().contains(&0x1b) && !json.as_bytes().contains(&0x01),
            "controls must not be printed directly"
        );
        // Display stays opaque rather than printing path bytes.
        assert_eq!(
            stored.workspace.to_string(),
            format!("<native path {} bytes>", stored.workspace.bytes.len())
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn local_coordination_unsupported_schema_and_corrupt_db() -> CoordResult<()> {
        let temp = tempfile::tempdir().unwrap();

        // A fully absent store is `None`, and reading creates nothing.
        let absent = paths_in(&temp.path().join("absent"));
        assert!(Store::open_read(&absent)?.is_none());
        assert!(!absent.root.exists());

        // A newer schema is unsupported and every byte stays untouched.
        let newer = paths_in(&temp.path().join("newer"));
        let mut store = Store::open_rw(&newer)?;
        store.insert(&sample_row(
            &uuid::Uuid::new_v4().to_string(),
            &hex64('a'),
            None,
            1_000,
        ))?;
        drop(store);
        raw_connection(&newer.database()).execute_batch("PRAGMA user_version=2")?;
        let newer_bytes = std::fs::read(newer.database()).unwrap();
        let newer_marker = std::fs::read(newer.root.join(DOMAIN_MARKER)).unwrap();
        assert!(matches!(
            Store::open_rw(&newer),
            Err(CoordinationError::UnsupportedSchema(2))
        ));
        assert!(matches!(
            Store::open_read(&newer),
            Err(CoordinationError::UnsupportedSchema(2))
        ));
        assert_eq!(std::fs::read(newer.database()).unwrap(), newer_bytes);
        assert_eq!(
            std::fs::read(newer.root.join(DOMAIN_MARKER)).unwrap(),
            newer_marker
        );

        // Invalid database bytes are reported without replacement.
        let corrupt = paths_in(&temp.path().join("corrupt"));
        Store::open_rw(&corrupt)?;
        std::fs::write(corrupt.database(), b"not a sqlite database").unwrap();
        let corrupt_bytes = std::fs::read(corrupt.database()).unwrap();
        let corrupt_marker = std::fs::read(corrupt.root.join(DOMAIN_MARKER)).unwrap();
        assert!(Store::open_rw(&corrupt).is_err());
        assert!(Store::open_read(&corrupt).is_err());
        assert_eq!(std::fs::read(corrupt.database()).unwrap(), corrupt_bytes);
        assert_eq!(
            std::fs::read(corrupt.root.join(DOMAIN_MARKER)).unwrap(),
            corrupt_marker
        );

        // A marker without its database is never re-initialized.
        let missing = paths_in(&temp.path().join("missing"));
        Store::open_rw(&missing)?;
        std::fs::remove_file(missing.database()).unwrap();
        let missing_marker = std::fs::read(missing.root.join(DOMAIN_MARKER)).unwrap();
        assert!(matches!(
            Store::open_rw(&missing),
            Err(CoordinationError::MissingDatabase)
        ));
        assert!(!missing.database().exists());
        assert_eq!(
            std::fs::read(missing.root.join(DOMAIN_MARKER)).unwrap(),
            missing_marker
        );

        // A database without its marker is refused; no marker is invented.
        let orphan = paths_in(&temp.path().join("orphan"));
        Store::open_rw(&orphan)?;
        std::fs::remove_file(orphan.root.join(DOMAIN_MARKER)).unwrap();
        let orphan_bytes = std::fs::read(orphan.database()).unwrap();
        assert!(matches!(
            Store::open_rw(&orphan),
            Err(CoordinationError::DomainMismatch)
        ));
        assert!(!orphan.root.join(DOMAIN_MARKER).exists());
        assert_eq!(std::fs::read(orphan.database()).unwrap(), orphan_bytes);

        // A marker/database identity mismatch is never repaired.
        let mismatch = paths_in(&temp.path().join("mismatch"));
        Store::open_rw(&mismatch)?;
        let replacement = uuid::Uuid::new_v4().to_string();
        std::fs::write(mismatch.root.join(DOMAIN_MARKER), replacement.as_bytes()).unwrap();
        let mismatch_bytes = std::fs::read(mismatch.database()).unwrap();
        assert!(matches!(
            Store::open_rw(&mismatch),
            Err(CoordinationError::DomainMismatch)
        ));
        assert!(matches!(
            Store::open_read(&mismatch),
            Err(CoordinationError::DomainMismatch)
        ));
        assert_eq!(
            std::fs::read(mismatch.root.join(DOMAIN_MARKER)).unwrap(),
            replacement.as_bytes()
        );
        assert_eq!(std::fs::read(mismatch.database()).unwrap(), mismatch_bytes);

        // Interrupted initialization (empty database, marker present) fails and
        // does not upgrade the empty file in place.
        let interrupted = paths_in(&temp.path().join("interrupted"));
        Store::open_rw(&interrupted)?;
        std::fs::write(interrupted.database(), b"").unwrap();
        let interrupted_marker = std::fs::read(interrupted.root.join(DOMAIN_MARKER)).unwrap();
        assert!(matches!(
            Store::open_rw(&interrupted),
            Err(CoordinationError::Corrupt)
        ));
        assert_eq!(
            std::fs::read(interrupted.database()).unwrap(),
            Vec::<u8>::new()
        );
        assert_eq!(
            std::fs::read(interrupted.root.join(DOMAIN_MARKER)).unwrap(),
            interrupted_marker
        );
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn local_coordination_concurrent_bootstrap() -> CoordResult<()> {
        use std::sync::{Arc, Barrier};

        let temp = tempfile::tempdir().unwrap();
        let paths = paths_in(temp.path());
        let barrier = Arc::new(Barrier::new(2));
        let mut handles = Vec::new();
        for _ in 0..2 {
            let paths = paths.clone();
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                barrier.wait();
                Store::open_rw_with(&paths, SCHEMA_LOCK_TIMEOUT, |root, paths| {
                    let started = Instant::now();
                    let opened = bootstrap(root, paths);
                    eprintln!("bootstrap under schema.lock took {:?}", started.elapsed());
                    opened
                })
                .map(|store| store.domain_id().to_string())
            }));
        }
        let first = handles.remove(0).join().unwrap();
        let second = handles.remove(0).join().unwrap();
        let (first, second) = (first?, second?);
        assert_eq!(first, second, "both initializers agree on one domain");

        let marker = std::fs::read_to_string(paths.root.join(DOMAIN_MARKER)).unwrap();
        assert_eq!(marker, first);
        let mut store = Store::open_rw(&paths)?;
        assert_eq!(store.domain_id(), first);
        let domains: i64 =
            store
                .connection()
                .query_row("SELECT COUNT(*) FROM domain_meta", [], |row| row.get(0))?;
        assert_eq!(domains, 1, "exactly one domain row exists");
        let version: i64 = store
            .connection()
            .query_row("PRAGMA user_version", [], |row| row.get(0))?;
        assert_eq!(version, 1);

        store.insert(&sample_row(
            &uuid::Uuid::new_v4().to_string(),
            &hex64('a'),
            None,
            1_000,
        ))?;
        assert_eq!(store.rows(10)?.len(), 1);
        assert_eq!(
            store.domain_id(),
            first,
            "the first identity is never rewritten"
        );
        drop(store);

        // A held lock yields `Busy`, never an empty-but-successful registry.
        paths.prepare_private().unwrap();
        let lock_file = std::fs::File::options()
            .read(true)
            .write(true)
            .open(paths.root.join(SCHEMA_LOCK))
            .unwrap();
        let held = crate::fs::OwnedFileLock::try_acquire(lock_file)
            .unwrap()
            .expect("the test holds the schema lock");
        assert!(matches!(
            Store::open_rw_with(&paths, Duration::from_millis(25), bootstrap),
            Err(CoordinationError::Busy)
        ));
        drop(held);
        let reopened = Store::open_read(&paths)?.expect("store remains readable");
        assert_eq!(reopened.rows(10)?.len(), 1);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn local_coordination_open_waits_for_slow_bootstrap() -> CoordResult<()> {
        use std::sync::mpsc;

        let temp = tempfile::tempdir().unwrap();
        let paths = paths_in(temp.path());
        let injected_delay = Duration::from_millis(2_500);
        let (ready, locked) = mpsc::channel();
        let (start, opening) = mpsc::channel();
        std::thread::scope(|scope| {
            let initializer_paths = &paths;
            let initializer = scope.spawn(move || {
                Store::open_rw_with(initializer_paths, SCHEMA_LOCK_TIMEOUT, |root, paths| {
                    ready.send(()).unwrap();
                    opening.recv_timeout(Duration::from_secs(10)).unwrap();
                    let started = Instant::now();
                    std::thread::sleep(injected_delay);
                    let opened = bootstrap(root, paths);
                    eprintln!(
                        "injected bootstrap under schema.lock took {:?}, result {:?}",
                        started.elapsed(),
                        opened.as_ref().map(Store::domain_id)
                    );
                    opened
                })
            });
            locked.recv_timeout(Duration::from_secs(10)).unwrap();
            let started = Instant::now();
            start.send(()).unwrap();
            let follower = Store::open_rw(&paths);
            let elapsed = started.elapsed();
            let initial = initializer.join().unwrap()?;
            eprintln!(
                "concurrent open took {elapsed:?}, result {:?}",
                follower.as_ref().map(Store::domain_id)
            );
            let follower = follower?;
            assert!(elapsed >= injected_delay);
            assert_eq!(follower.domain_id(), initial.domain_id());
            check_schema(follower.connection(), initial.domain_id())?;
            Ok(())
        })
    }
}
