//! Direct fixture inspection and fault injection for a published test store.
#![allow(dead_code)]
use anyhow::{ensure, Result};
use deepseek_code_core::db::session_store::{settings::Setting, SessionAccess, Stores};
use rusqlite::{Connection, OpenFlags};
use serde::{de::DeserializeOwned, Serialize};
use std::path::{Path, PathBuf};

fn existing(path: &Path) -> Result<Connection> {
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    db.pragma_update(None, "foreign_keys", true)?;
    Ok(db)
}

pub fn shard_connection(root: &Path, id: &str) -> Result<Connection> {
    let stores = Stores::open(root)?;
    existing(&stores.shard_path(id)?)
}

pub fn register_child(stores: &Stores, parent: &str, child: &str) {
    use deepseek_code_core::{
        db::session_store::WorkerDirectory, native_engine::child_session::ChildOwnership,
    };
    let now = chrono::Utc::now().to_rfc3339();
    let pair = stores.worker_pair(parent, child).unwrap();
    stores
        .save_child_owner(
            &pair,
            &ChildOwnership {
                parent_session_id: parent.into(),
                agent_type: "general".into(),
                description: "Fixture child".into(),
                mode: "foreground".into(),
                status: "running".into(),
                job_id: None,
                created_at: now.clone(),
                updated_at: now,
                last_error: None,
            },
        )
        .unwrap();
}

pub trait FixtureSql {
    fn seed_preference<T: DeserializeOwned + Serialize>(
        &self,
        id: &str,
        key: Setting<T>,
        value: &impl Serialize,
    ) -> Result<()>;
    fn fixture_preference<T: DeserializeOwned + Serialize>(
        &self,
        id: &str,
        key: Setting<T>,
    ) -> Result<Option<serde_json::Value>>;
    fn pending_input(&self, id: &str, matches: impl Fn(&str) -> bool) -> Result<bool>;
    fn directory_sql<T>(&self, body: impl FnOnce(&Connection) -> T) -> Result<T>;
    fn shard_path(&self, id: &str) -> Result<PathBuf>;
    fn shard_sql<T>(&self, id: &str, body: impl FnOnce(&Connection) -> T) -> Result<T>;
}

impl FixtureSql for Stores {
    fn seed_preference<T: DeserializeOwned + Serialize>(
        &self,
        id: &str,
        key: Setting<T>,
        value: &impl Serialize,
    ) -> Result<()> {
        let value: T = serde_json::from_value(serde_json::to_value(value)?)?;
        self.select_session(id)?.set_preference(key, Some(&value))
    }
    fn fixture_preference<T: DeserializeOwned + Serialize>(
        &self,
        id: &str,
        key: Setting<T>,
    ) -> Result<Option<serde_json::Value>> {
        self.select_session(id)?
            .preference(key)?
            .map(serde_json::to_value)
            .transpose()
            .map_err(Into::into)
    }
    fn pending_input(&self, id: &str, matches: impl Fn(&str) -> bool) -> Result<bool> {
        use deepseek_code_core::services::input_queue::QueueState;
        Ok(self.open_session(id)?.queue_items()?.iter().any(|row| {
            row.state == QueueState::Queued && row.content.as_str().is_some_and(&matches)
        }))
    }
    fn directory_sql<T>(&self, body: impl FnOnce(&Connection) -> T) -> Result<T> {
        let db = existing(&self.data_dir().join("global.db"))?;
        Ok(body(&db))
    }
    fn shard_path(&self, id: &str) -> Result<PathBuf> {
        let incarnation = self.open_session(id)?.incarnation().to_owned();
        let key: String = self.directory_sql(|c| {
            c.query_row(
                "SELECT storage_key FROM conversations WHERE id=?1 AND incarnation=?2",
                [id, &incarnation],
                |r| r.get(0),
            )
        })??;
        ensure!(
            Path::new(&key).file_name() == Some(key.as_ref()),
            "Invalid fixture storage key"
        );
        Ok(self
            .data_dir()
            .join("data/sessions")
            .join(key)
            .join("session.db"))
    }
    fn shard_sql<T>(&self, id: &str, body: impl FnOnce(&Connection) -> T) -> Result<T> {
        let db = existing(&self.shard_path(id)?)?;
        ensure!(
            db.query_row("SELECT id FROM manifest", [], |r| r.get::<_, String>(0))? == id,
            "Wrong fixture shard"
        );
        Ok(body(&db))
    }
}
