//! Global definitions and incarnation-scoped copies bound to individual sessions.
use super::{layout, shared::DirectoryRepository, GlobalSetting, SessionRepository, Stores};
use crate::db::DbManager;
use crate::services::agents::Definition;
use anyhow::{ensure, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};

pub trait AgentCatalog {
    fn definitions(&self) -> Result<Vec<Definition>>;
    fn save_definition(
        &self,
        definition: &Definition,
        replace: bool,
        preview: bool,
    ) -> Result<bool>;
    fn backup_definitions(&self, root: &Path) -> Result<PathBuf>;
}
trait AgentSettings: DirectoryRepository {
    const SPLIT: bool;
}
impl AgentSettings for Stores {
    const SPLIT: bool = true;
}
impl AgentSettings for DbManager {
    const SPLIT: bool = false;
}

fn raw(c: &Connection, split: bool) -> Result<Option<String>> {
    let query = if split {
        "SELECT value FROM global_settings WHERE domain='agent' AND name='agent-definitions'"
    } else {
        "SELECT value FROM peripheral_settings WHERE key='agent-definitions'"
    };
    Ok(c.query_row(query, [], |r| r.get(0)).optional()?)
}
fn decode(value: Option<&str>) -> Result<Vec<Definition>> {
    Ok(value
        .map(serde_json::from_str)
        .transpose()?
        .unwrap_or_default())
}
impl<D: AgentSettings> AgentCatalog for D {
    fn definitions(&self) -> Result<Vec<Definition>> {
        decode(self.directory_read(|c| raw(c, D::SPLIT))?.as_deref())
    }
    fn save_definition(
        &self,
        definition: &Definition,
        replace: bool,
        preview: bool,
    ) -> Result<bool> {
        let started = std::time::Instant::now();
        loop {
            let previous = self.directory_read(|c| raw(c, D::SPLIT))?;
            let mut definitions = decode(previous.as_deref())?;
            if let Some(i) = definitions.iter().position(|d| d.id == definition.id) {
                if !replace {
                    ensure!(
                        definitions[i] == *definition,
                        "Agent ID {} conflicts with an existing definition",
                        definition.id
                    );
                    return Ok(false);
                }
                definitions.remove(i);
            }
            if preview {
                return Ok(true);
            }
            definitions.push(definition.clone());
            let next = serde_json::to_string(&definitions)?;
            let remaining = super::WRITE_DEADLINE.saturating_sub(started.elapsed());
            ensure!(
                !remaining.is_zero(),
                "Agent definitions changed during update"
            );
            let saved=self.directory_write("save agent definition",&definition.id,remaining,|tx|{
                if raw(tx,D::SPLIT)? != previous {return Ok(false);}
                let sql=if D::SPLIT {"INSERT INTO global_settings VALUES('agent','agent-definitions',?1) ON CONFLICT(domain,name) DO UPDATE SET value=excluded.value"}
                    else {"INSERT INTO peripheral_settings VALUES('agent-definitions',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value"};
                tx.execute(sql,[&next])?;
                Ok(true)
            })?;
            if saved {
                return Ok(true);
            }
        }
    }
    fn backup_definitions(&self, root: &Path) -> Result<PathBuf> {
        if !D::SPLIT {
            let backups = crate::data_layout::DataLayout::from_root(&root).backups();
            std::fs::create_dir_all(&backups)?;
            let backups = backups.canonicalize()?;
            let path = backups.join(format!("agents-{}.sqlite3", uuid::Uuid::new_v4()));
            self.directory_read(|c| {
                c.execute("VACUUM INTO ?1", [path.to_string_lossy().as_ref()])?;
                Ok(())
            })?;
            return Ok(path);
        }
        let directory = crate::data_layout::DataLayout::from_root(&root)
            .backups()
            .join(format!("agents-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory)?;
        let directory = directory.canonicalize()?;
        let file = directory.join("global.db");
        self.directory_read(|c| {
            c.execute("VACUUM INTO ?1", [file.to_string_lossy().as_ref()])?;
            Ok(())
        })?;
        std::fs::File::options()
            .write(true)
            .open(&file)?
            .sync_all()?;
        let manifest = directory.join("manifest.json");
        let [domain, name] = GlobalSetting::AgentDefinition("agent-definitions").parts();
        std::fs::write(
            &manifest,
            serde_json::to_vec(
                &serde_json::json!({"database":"global.db","settings":[{"domain":domain,"name":name}],"session_shards":[]}),
            )?,
        )?;
        std::fs::File::options()
            .write(true)
            .open(&manifest)?
            .sync_all()?;
        layout::sync_dir(&directory)?;
        layout::sync_dir(directory.parent().unwrap())?;
        layout::sync_dir(root)?;
        Ok(manifest)
    }
}
impl SessionRepository<'_> {
    pub(crate) fn bind_agent(&self, definition: &Definition) -> Result<()> {
        let raw = serde_json::to_string(definition)?;
        self.write("agent binding",|c,id|{
            c.execute("INSERT INTO session_agents(conversation_id,definition) VALUES(?1,?2) ON CONFLICT(conversation_id) DO UPDATE SET definition=excluded.definition",params![id,raw])?;
            Ok(())
        })
    }
    pub(crate) fn bound_agent(&self) -> Result<Option<Definition>> {
        let raw: Option<String> = self.read(|c, id| {
            Ok(c.query_row(
                "SELECT definition FROM session_agents WHERE conversation_id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()?)
        })?;
        Ok(raw.as_deref().map(serde_json::from_str).transpose()?)
    }
}
