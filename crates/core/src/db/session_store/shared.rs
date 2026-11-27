//! Common implementation boundary for repositories whose rows belong entirely to G.
use super::Stores;
use anyhow::Result;
use rusqlite::{Connection, Transaction};
use std::time::Duration;

pub(super) trait DirectoryRepository {
    fn directory_read<T>(&self, body: impl FnOnce(&Connection) -> Result<T>) -> Result<T>;
    fn directory_write<T>(
        &self,
        label: &str,
        owner: &str,
        deadline: Duration,
        body: impl FnMut(&Transaction<'_>) -> Result<T>,
    ) -> Result<T>;
}
impl DirectoryRepository for Stores {
    fn directory_read<T>(&self, body: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.global().read(body)
    }
    fn directory_write<T>(
        &self,
        label: &str,
        owner: &str,
        deadline: Duration,
        mut body: impl FnMut(&Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        let start = std::time::Instant::now();
        self.global()
            .connection(rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE, |c| {
                c.pragma_update(None, "synchronous", "NORMAL")?;
                crate::db::write_tx(c, label, deadline, |tx| body(tx))
            })
            .map_err(|e| crate::db::write_boundary_context(label, owner, start, e))
    }
}
impl DirectoryRepository for crate::db::DbManager {
    fn directory_read<T>(&self, body: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        self.with_conn(body)?
    }
    fn directory_write<T>(
        &self,
        label: &str,
        _owner: &str,
        deadline: Duration,
        mut body: impl FnMut(&Transaction<'_>) -> Result<T>,
    ) -> Result<T> {
        self.with_conn(|c| crate::db::write_tx(c, label, deadline, |tx| body(tx)))?
    }
}

use super::layout;
use anyhow::ensure;
use std::path::{Path, PathBuf};

impl Stores {
    pub(super) fn backup_catalog(
        &self,
        root: &Path,
        name: &str,
        tables: &[&str],
        refs_sql: &str,
    ) -> Result<PathBuf> {
        ensure!(
            root.canonicalize()? == self.data_dir(),
            "Catalog backup data root changed"
        );
        let backups = crate::data_layout::DataLayout::from_root(&root).backups();
        std::fs::create_dir_all(&backups)?;
        let backups = backups.canonicalize()?;
        let staged = tempfile::tempdir_in(&backups)?;
        let db = staged.path().join("global.db");
        self.directory_read(|c| {
            c.execute("VACUUM INTO ?1", [db.to_string_lossy().as_ref()])?;
            Ok(())
        })?;
        std::fs::File::options().write(true).open(&db)?.sync_all()?;
        let refs = {
            let c = layout::open(&db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            let mut q = c.prepare(refs_sql)?;
            let rows = q.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let blobs = staged.path().join("data/global-blobs");
        std::fs::create_dir_all(&blobs)?;
        for digest in &refs {
            use sha2::{Digest, Sha256};
            ensure!(
                digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
                "Invalid catalog digest"
            );
            let bytes = std::fs::read(
                crate::data_layout::DataLayout::from_root(&root)
                    .global_blobs()
                    .join(digest),
            )?;
            ensure!(
                format!("{:x}", Sha256::digest(&bytes)) == *digest,
                "Catalog backup digest mismatch"
            );
            let path = blobs.join(digest);
            std::fs::write(&path, bytes)?;
            std::fs::File::options()
                .write(true)
                .open(path)?
                .sync_all()?;
        }
        layout::sync_dir(&blobs)?;
        let manifest = staged.path().join("manifest.json");
        std::fs::write(
            &manifest,
            serde_json::to_vec(
                &serde_json::json!({"database":"global.db","tables":tables,"blobs":refs,"session_shards":[]}),
            )?,
        )?;
        std::fs::File::options()
            .write(true)
            .open(&manifest)?
            .sync_all()?;
        layout::sync_dir(staged.path())?;
        let dest = backups.join(format!("{name}-{}", uuid::Uuid::new_v4()));
        layout::durable_move(staged.path(), &dest)?;
        layout::sync_dir(&backups)?;
        layout::sync_dir(root)?;
        Ok(dest.join("manifest.json"))
    }
}

/// Stage immutable bodies before opening the short directory transaction.
pub(super) trait CatalogBodies: DirectoryRepository {
    fn store_body(&self, text: &str) -> Result<String>;
    fn body_reference(&self, text: &str) -> Result<String>;
    fn load_body(&self, reference: &str) -> Result<String>;
    fn catalog_backup(
        &self,
        root: &Path,
        name: &str,
        tables: &[&str],
        references: &str,
    ) -> Result<PathBuf>;
}
impl CatalogBodies for Stores {
    fn store_body(&self, text: &str) -> Result<String> {
        self.put_payload(&serde_json::Value::String(text.into()))
    }
    fn body_reference(&self, text: &str) -> Result<String> {
        use sha2::{Digest, Sha256};
        Ok(format!("{:x}", Sha256::digest(serde_json::to_vec(text)?)))
    }
    fn load_body(&self, reference: &str) -> Result<String> {
        Ok(serde_json::from_value(self.payload(reference)?)?)
    }
    fn catalog_backup(
        &self,
        root: &Path,
        name: &str,
        tables: &[&str],
        references: &str,
    ) -> Result<PathBuf> {
        self.backup_catalog(root, name, tables, references)
    }
}
impl CatalogBodies for crate::db::DbManager {
    fn store_body(&self, text: &str) -> Result<String> {
        Ok(text.into())
    }
    fn body_reference(&self, text: &str) -> Result<String> {
        Ok(text.into())
    }
    fn load_body(&self, raw: &str) -> Result<String> {
        Ok(raw.into())
    }
    fn catalog_backup(
        &self,
        root: &Path,
        name: &str,
        _tables: &[&str],
        _references: &str,
    ) -> Result<PathBuf> {
        let dir = crate::data_layout::DataLayout::from_root(&root).backups();
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{name}-{}.sqlite3", uuid::Uuid::new_v4()));
        self.directory_read(|c| {
            c.execute("VACUUM INTO ?1", [path.to_string_lossy().as_ref()])?;
            Ok(())
        })?;
        Ok(path)
    }
}
