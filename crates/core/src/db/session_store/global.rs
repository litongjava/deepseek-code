use super::layout::{self, GLOBAL};
use anyhow::{ensure, Result};
use r2d2::{ManageConnection, Pool};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction};
use std::path::{Path, PathBuf};
use std::sync::Weak;
use std::time::Instant;

// One pool budget for all access modes. A slot closes its old descriptor
// before switching between an OS read-only connection and a writer.
struct Slots;
type Slot = Option<(OpenFlags, Connection)>;
impl ManageConnection for Slots {
    type Connection = Slot;
    type Error = rusqlite::Error;
    fn connect(&self) -> rusqlite::Result<Slot> {
        Ok(None)
    }
    fn is_valid(&self, _: &mut Slot) -> rusqlite::Result<()> {
        Ok(())
    }
    fn has_broken(&self, _: &mut Slot) -> bool {
        false
    }
}

pub struct GlobalDb {
    pub(super) root: Weak<super::Root>,
    #[cfg(unix)]
    identity: (u64, u64),
    path: PathBuf,
    pub(super) uuid: String,
    pool: Pool<Slots>,
    verified_schema: std::sync::Mutex<Option<i64>>,
}

impl GlobalDb {
    pub(super) fn new(root: &Path, uuid: String) -> Result<Self> {
        Ok(Self {
            root: Weak::new(),
            #[cfg(unix)]
            identity: file_identity(&root.join("global.db"))?,
            path: root.join("global.db"),
            uuid,
            verified_schema: Default::default(),
            pool: Pool::builder().max_size(4).min_idle(Some(0)).build(Slots)?,
        })
    }

    pub(super) fn connection<T>(
        &self,
        flags: OpenFlags,
        body: impl FnOnce(&Connection) -> Result<T>,
    ) -> Result<T> {
        self.checkout(flags, false, body)
    }

    pub(super) fn checkout<T>(
        &self,
        flags: OpenFlags,
        nonblocking: bool,
        body: impl FnOnce(&Connection) -> Result<T>,
    ) -> Result<T> {
        layout::allow_entry()?;
        let mut slot = if nonblocking {
            self.pool
                .try_get()
                .ok_or_else(|| anyhow::anyhow!("Global pool busy"))?
        } else {
            self.pool.get()?
        };
        #[cfg(unix)]
        ensure!(
            file_identity(&self.path)? == self.identity,
            "Global database was replaced"
        );
        // Without a portable inode, discard the cached descriptor and check
        // the UUID on a fresh open of the current pathname.
        #[cfg(not(unix))]
        {
            *slot = None;
        }
        ensure!(
            std::fs::read_to_string(
                crate::data_layout::DataLayout::from_root(self.path.parent().unwrap())
                    .lock_path("session")
                    .join(".initialized")
            )? == self.uuid,
            "Global store UUID changed"
        );
        if slot.as_ref().map(|(mode, _)| *mode) != Some(flags) {
            *slot = None;
            *slot = Some((
                flags,
                layout::open_timed(&self.path, flags, if nonblocking { 0 } else { 5000 })?,
            ));
        }
        let conn = &slot.as_ref().unwrap().1;
        conn.busy_timeout(std::time::Duration::from_millis(if nonblocking {
            0
        } else {
            5000
        }))?;
        let (kind, version, schema, journal): (i32, i32, i64, String) = conn.query_row(
            "SELECT application_id,user_version,schema_version,journal_mode FROM pragma_application_id,
             pragma_user_version,pragma_schema_version,pragma_journal_mode", [],
            |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))
        )?;
        ensure!(
            kind == GLOBAL && version == layout::GLOBAL_VERSION && journal == "wal",
            "Unsupported global database format"
        );
        {
            let mut verified = self.verified_schema.lock().unwrap();
            if *verified != Some(schema) {
                layout::verify(conn, GLOBAL)?;
                *verified = Some(schema);
            }
        }
        let store: String = conn.query_row(
            "SELECT store_uuid FROM store_identity WHERE singleton=1",
            [],
            |r| r.get(0),
        )?;
        ensure!(store == self.uuid, "Global store identity changed");
        layout::with_checkout("global checkout", || body(conn))
    }

    pub(super) fn read<T>(&self, body: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.connection(OpenFlags::SQLITE_OPEN_READ_ONLY, body)
    }

    pub(super) fn write<T>(
        &self,
        label: &str,
        id: &str,
        publication: bool,
        body: impl FnMut(&Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        self.write_with_deadline(label, id, publication, super::WRITE_DEADLINE, body)
    }

    pub(super) fn write_with_deadline<T>(
        &self,
        label: &str,
        id: &str,
        publication: bool,
        deadline: std::time::Duration,
        mut body: impl FnMut(&Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        let start = Instant::now();
        self.connection(OpenFlags::SQLITE_OPEN_READ_WRITE, |conn| {
            conn.pragma_update(
                None,
                "synchronous",
                if publication { "FULL" } else { "NORMAL" },
            )?;
            crate::db::write_tx(
                conn,
                label,
                deadline.saturating_sub(start.elapsed()),
                |tx| body(tx),
            )
        })
        .map_err(|e| crate::db::write_boundary_context(label, id, start, e))
    }

    pub(super) fn incarnation(&self, id: &str, key: &str) -> Result<Option<String>> {
        self.read(|c| {
            Ok(c.query_row(
                "SELECT incarnation FROM conversations WHERE id=?1 AND storage_key=?2",
                [id, key],
                |r| r.get(0),
            )
            .optional()?)
        })
    }
    pub(super) fn read_directory_row<T>(
        &self,
        id: &str,
        read: impl FnOnce(&Connection) -> Result<Option<T>>,
    ) -> Result<Option<T>> {
        let key = layout::storage_key(id)?;
        let stores = super::Stores {
            root: self.root.upgrade().expect("global belongs to a live root"),
            fault: None,
            selected: None, router: None,
        };
        let Some(_locks) = stores.resolved_key(&key)? else {
            return Ok(None);
        };
        self.read(read)
    }
    #[doc(hidden)]
    pub fn with_read_for_test<T>(&self, body: impl FnOnce() -> T) -> Result<T> {
        self.read(|_| Ok(body()))
    }
    pub fn store_uuid(&self) -> &str {
        &self.uuid
    }
}

#[cfg(unix)]
fn file_identity(path: &Path) -> Result<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(metadata.is_file(), "Global database must be a regular file");
    Ok((metadata.dev(), metadata.ino()))
}
