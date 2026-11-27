//! Host-bound project memories. Legacy SQLite remains an explicit archive, never
//! an implicit source for prompt context. No model argument can select a scope.
pub(crate) mod prompt;
use super::ServiceContext;
use crate::db::session_store::{settings, GlobalSetting, Projects, SessionDb};
use crate::memory::files::{ExpectedVersion, FileMemoryStore, MemoryScope};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Component, Path, PathBuf};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Selection {
    pub project_id: Option<String>,
    pub session_id: Option<String>,
}

#[derive(Clone)]
pub struct FileMemory {
    pub context: ServiceContext,
    pub scope: MemoryScope,
    pub directory: PathBuf,
    session: Option<SessionDb>,
    private_owner: Option<String>,
}

/// The directory database owns file-mode preferences. Import existing archive
/// preferences read-only when upgrading a data directory from the old layout.
pub fn config(context: &ServiceContext) -> Result<crate::memory::MemoryConfig> {
    let key = GlobalSetting::MachinePreference("memory.files.config");
    if let Some(config) = context.global_setting(key)? {
        return Ok(config);
    }
    let imported = archive_config(&context.paths.data_dir)?.unwrap_or_default();
    context.db.global().initialize_setting(key, &imported)
}

fn archive_config(data_dir: &Path) -> Result<Option<crate::memory::MemoryConfig>> {
    let archive = data_dir.join("data/memory.sqlite3");
    if archive.is_file() {
        use rusqlite::{Connection, OpenFlags, OptionalExtension};
        let conn = Connection::open_with_flags(archive, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        if conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='memory_settings')", [], |row| row.get::<_, bool>(0))? {
            let raw: Option<String> = conn.query_row("SELECT config FROM memory_settings WHERE id=1", [], |r| r.get(0)).optional()?;
            if let Some(raw) = raw {
                return Ok(Some(serde_json::from_str(&raw)?));
            }
        }
    }
    Ok(None)
}

pub fn save_config(context: &ServiceContext, config: &crate::memory::MemoryConfig) -> Result<()> {
    config.validate()?;
    context.save_global_setting(
        GlobalSetting::MachinePreference("memory.files.config"),
        config,
    )
}

/// Only the user-level settings file can grant access to a shared memory directory.
fn team_directory(data: &Path) -> Result<Option<PathBuf>> {
    let raw = match std::fs::read(
        crate::data_layout::DataLayout::from_root(&data).config_file("settings.json"),
    ) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        other => other?,
    };
    let settings: Value = serde_json::from_slice(&raw)?;
    let directory: Option<PathBuf> =
        serde_json::from_value(settings["memory"]["team_dir"].clone())?;
    ensure!(
        directory.as_ref().is_none_or(|d| d.is_absolute()),
        "memory.team_dir must be absolute"
    );
    Ok(directory)
}

fn private_roots(roots: &[PathBuf]) -> bool {
    roots.is_empty()
        || (roots.len() == 1
            && dirs::home_dir()
                .is_some_and(|home| home.canonicalize().ok() == roots[0].canonicalize().ok()))
}

impl FileMemory {
    pub fn select(context: &ServiceContext, selection: &Selection) -> Result<Self> {
        ensure!(
            selection.project_id.is_some() ^ selection.session_id.is_some(),
            "Select exactly one memory project or session scope"
        );
        if let Some(session) = &selection.session_id {
            return Self::session(context, session);
        }
        let id = selection.project_id.as_deref().unwrap();
        let project = context.db.trusted_project(id)?;
        ensure!(
            !project.is_archived,
            "Archived project memory is unavailable"
        );
        let roots = project
            .folders
            .iter()
            .map(|f| PathBuf::from(&f.path))
            .collect::<Vec<_>>();
        ensure!(
            !(roots.len() == 1
                && dirs::home_dir()
                    .is_some_and(|h| roots[0].canonicalize().ok() == h.canonicalize().ok())),
            "Home memory requires a session scope"
        );
        Self::new(context, MemoryScope::project(&roots, true)?, None)
    }

    pub fn session(context: &ServiceContext, id: &str) -> Result<Self> {
        let shard = context.db.open_session(id)?;
        Self::from_session(context, shard, None)
    }

    fn from_session(
        context: &ServiceContext,
        shard: SessionDb,
        inherited: Option<&str>,
    ) -> Result<Self> {
        let (workspace, project, _) = shard.scoped_stores().engine_workspace(
            &context.paths.data_dir.join("data/workspaces"),
            shard.id(),
            None,
        )?;
        let project_id = project.as_ref().map(|p| p.id.clone());
        let roots: Vec<_> = project
            .iter()
            .flat_map(|p| &p.folders)
            .map(|f| PathBuf::from(&f.path))
            .collect();
        let private = private_roots(&roots);
        let mut private_owner = None;
        let scope = if private {
            let saved = shard.read(|r| r.setting(settings::MEMORY_OWNER))?;
            let default_owner = if roots.is_empty() {
                workspace
                    .file_name()
                    .and_then(|s| s.to_str())
                    .context("Missing private workspace owner")?
            } else {
                shard.incarnation()
            };
            let owner = inherited
                .or_else(|| {
                    saved
                        .as_ref()
                        .filter(|(project, _)| project == &project_id)
                        .map(|(_, owner)| owner.as_str())
                })
                .unwrap_or(default_owner);
            private_owner = Some(owner.to_owned());
            MemoryScope::private(owner)?
        } else {
            MemoryScope::project(&roots, true)?
        };
        let binding = (project_id.clone(), scope.key().to_owned());
        let saved = shard.read(|r| r.setting(settings::MEMORY_SCOPE))?;
        if saved.as_ref() != Some(&binding) {
            shard.write(|w| {
                if let Some((project, key)) = w.setting(settings::MEMORY_SCOPE)? {
                    ensure!(
                        project != project_id || key == scope.key(),
                        "Memory scope changed; explicitly rebind the session before using memory"
                    );
                }
                if let (Some(_), Some(owner)) = (inherited, &private_owner) {
                    w.set_setting(settings::MEMORY_OWNER, &(project_id.clone(), owner.clone()))?;
                }
                w.set_setting(settings::MEMORY_SCOPE, &binding)
            })?;
        }
        let mut memory = Self::new(context, scope, Some(shard))?;
        memory.private_owner = private_owner;
        Ok(memory)
    }

    /// Called only by the owned-child launcher, after it persists the child.
    pub fn child(&self, id: &str) -> Result<Self> {
        Self::from_session(
            &self.context,
            self.context.db.open_session(id)?,
            self.private_owner.as_deref(),
        )
    }

    /// CLI management uses an explicitly selected cwd, never arbitrary model text.
    pub fn workspace(context: &ServiceContext, root: &Path) -> Result<Self> {
        let home = dirs::home_dir();
        ensure!(
            home.as_ref()
                .is_none_or(|h| root.canonicalize().ok() != h.canonicalize().ok()),
            "Home memory requires --session; select a project directory otherwise"
        );
        Self::new(
            context,
            MemoryScope::project(&[root.to_path_buf()], true)?,
            None,
        )
    }

    fn new(
        context: &ServiceContext,
        scope: MemoryScope,
        session: Option<SessionDb>,
    ) -> Result<Self> {
        let root = context.paths.data_dir.canonicalize()?;
        let directory = root
            .join("data/memory/projects")
            .join(scope.key())
            .join("memory");
        // This changes service state only; old data and dream configuration stay.
        if !context
            .global_setting::<bool>(GlobalSetting::MachinePreference("memory.files.active"))?
            .unwrap_or(false)
        {
            context.save_global_setting(
                GlobalSetting::MachinePreference("memory.files.active"),
                &true,
            )?;
        }
        Ok(Self {
            context: context.clone(),
            scope,
            directory,
            session,
            private_owner: None,
        })
    }

    fn directories(
        &self,
        team: Option<&Path>,
        agent: Option<&str>,
        session: &Path,
    ) -> Result<Vec<(&'static str, PathBuf)>> {
        let root = self.context.paths.data_dir.canonicalize()?;
        let mut dirs = vec![(
            "global",
            crate::data_layout::DataLayout::from_root(&root)
                .memory_files()
                .join("global"),
        )];
        if let Some(team) = team {
            ensure!(team.is_absolute(), "Team memory directory must be absolute");
            dirs.push(("team", team.to_owned()));
        }
        dirs.push(("project", self.directory.clone()));
        if let Some(agent) = agent {
            ensure!(
                !agent.is_empty()
                    && agent
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_')),
                "Invalid memory agent definition"
            );
            dirs.push((
                "subagent",
                self.directory.parent().unwrap().join("agents").join(agent),
            ));
        }
        dirs.push(("session", session.to_owned()));
        Ok(dirs)
    }

    fn session_directories(&self) -> Result<Vec<(&'static str, PathBuf)>> {
        let Some(session) = &self.session else {
            return Ok(vec![("project", self.directory.clone())]);
        };
        let agent = session.read(|r| r.setting(settings::MEMORY_AGENT))?;
        self.directories(
            team_directory(&self.context.paths.data_dir)?.as_deref(),
            agent.as_deref(),
            &session.memory_directory(),
        )
    }

    pub fn active(&self) -> bool {
        config(&self.context).is_ok_and(|config| config.enabled)
    }

    fn validate_scope(&self) -> Result<()> {
        ensure!(self.active(), "Local file memory is disabled");
        if let Some(id) = &self.session {
            let current = Self::from_session(&self.context, id.clone(), None)?;
            ensure!(
                current.scope.key() == self.scope.key(),
                "Memory scope changed during the turn; restart the turn"
            );
        }
        Ok(())
    }

    pub fn store(&self, read_only: bool) -> Result<FileMemoryStore> {
        self.validate_scope()?;
        Ok(if read_only {
            FileMemoryStore::open_read_only(&self.context.paths.data_dir, &self.scope)?
        } else {
            FileMemoryStore::open(&self.context.paths.data_dir, &self.scope)?
        })
    }

    fn snapshot(&self) -> Result<crate::memory::files::Snapshot> {
        self.validate_scope()?;
        Ok(FileMemoryStore::project_snapshot(
            &self.context.paths.data_dir,
            &self.scope,
        )?)
    }

    pub fn repair_catalog(&self, confirmation: Option<&str>) -> Result<Value> {
        let snapshot = self.snapshot()?;
        let (proposed, token) = snapshot.repair_preview(self.scope.key())?;
        let changed = proposed != snapshot.index;
        if let Some(confirmation) = confirmation {
            ensure!(
                token == confirmation,
                "Catalog repair preview changed; preview again"
            );
            if changed {
                self.store(false)?
                    .confirm_repair(self.scope.key(), confirmation)?;
            }
            return Ok(json!({"applied":changed,"scope":self.scope.key()}));
        }
        Ok(
            json!({"scope":self.scope.key(),"changed":changed,"before":snapshot.index,"after":proposed,"confirmation":token}),
        )
    }

    pub fn listing(&self, query: &str) -> Result<Value> {
        let snapshot = self.snapshot()?;
        let items = snapshot.entries.iter().filter(|entry| query.is_empty() || relevance(&entry.document, query) > 0.0).map(|entry| json!({
            "id":entry.filename,"name":entry.document.name,"description":entry.document.description,"type":entry.document.kind,
            "content":String::from_utf8_lossy(&entry.bytes),"version":entry.hash
        })).collect::<Vec<_>>();
        let index = snapshot.index_context(&self.directory.join("MEMORY.md").to_string_lossy());
        Ok(
            json!({"schema_version":2,"scope":self.scope.key(),"directory":self.directory,"generation":snapshot.generation,"items":items,
            "index":snapshot.index,"version":snapshot.index_hash,"omitted":index.omitted,
            "diagnostics":snapshot.diagnostics.iter().map(|d|json!({"file":d.filename,"message":d.message})).collect::<Vec<_>>()}),
        )
    }

    pub fn put(&self, file: &str, content: &str, version: Option<&str>) -> Result<Value> {
        let expected = version
            .map(|v| ExpectedVersion::Hash(v.into()))
            .unwrap_or(ExpectedVersion::Absent);
        let store = self.store(false)?;
        let entry = store.put(file, content.as_bytes(), expected)?;
        let snapshot = store.snapshot()?;
        let index = snapshot.index_context(&self.directory.join("MEMORY.md").to_string_lossy());
        Ok(
            json!({"schema_version":2,"saved":true,"id":entry.filename,"version":entry.hash,"scope":self.scope.key(),"index_over_budget":index.truncated,"omitted":index.omitted}),
        )
    }

    pub fn delete(&self, file: &str, version: &str) -> Result<Value> {
        self.store(false)?
            .delete(file, ExpectedVersion::Hash(version.into()))?;
        Ok(json!({"schema_version":2,"deleted":file,"scope":self.scope.key()}))
    }

    /// Return a single legal managed filename, None for an unrelated target.
    /// Any attempt into the memory tree outside the current root fails closed.
    pub fn target(&self, tool: &str, input: &Value, cwd: &str) -> Result<Option<String>> {
        let mut paths = Vec::new();
        if tool == "ApplyPatch" {
            let patch = input["patch"].as_str().unwrap_or_default();
            for line in patch.lines() {
                for prefix in [
                    "*** Delete File: ",
                    "*** Add File: ",
                    "*** Update File: ",
                    "*** Move to: ",
                ] {
                    if let Some(p) = line.strip_prefix(prefix) {
                        paths.push(p);
                    }
                }
            }
        } else if matches!(
            tool,
            "Read" | "Write" | "Edit" | "MultiEdit" | "NotebookEdit"
        ) {
            if let Some(p) = input["file_path"]
                .as_str()
                .or(input["notebook_path"].as_str())
            {
                paths.push(p);
            }
        }
        let base = self
            .directory
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap();
        let directories = self.session_directories()?;
        let sessions = self.context.paths.data_dir.join("data/sessions");
        let protected = |path: &Path| {
            path.starts_with(base)
                || directories.iter().any(|(_, dir)| path.starts_with(dir))
                || path
                    .strip_prefix(&sessions)
                    .is_ok_and(|tail| tail.iter().nth(1).is_some_and(|name| name == "memory"))
        };
        let mut managed = None;
        for raw in &paths {
            let path = Path::new(raw);
            let absolute = if path.is_absolute() {
                path.to_path_buf()
            } else {
                Path::new(cwd).join(path)
            };
            let mut normalized = PathBuf::new();
            for c in absolute.components() {
                match c {
                    Component::ParentDir => {
                        normalized.pop();
                    }
                    Component::CurDir => {}
                    c => normalized.push(c.as_os_str()),
                }
            }
            // Resolve the nearest existing ancestor as well: a new filename
            // under an outside symlink alias must not bypass managed I/O.
            let resolved = normalized
                .ancestors()
                .find_map(|ancestor| {
                    ancestor
                        .canonicalize()
                        .ok()
                        .map(|real| real.join(normalized.strip_prefix(ancestor).unwrap()))
                })
                .unwrap_or_else(|| normalized.clone());
            if !protected(&normalized) && !protected(&resolved) {
                continue;
            }
            ensure!(
                !absolute
                    .components()
                    .any(|c| matches!(c, Component::ParentDir)),
                "Memory path traversal is forbidden"
            );
            let (_, directory) = directories
                .iter()
                .find(|(_, dir)| {
                    normalized.parent().is_some_and(|parent| {
                        parent
                            .canonicalize()
                            .ok()
                            .zip(dir.canonicalize().ok())
                            .is_some_and(|(left, right)| left == right)
                            || parent == dir.as_path()
                    })
                })
                .context("Only the current session's memory scopes are accessible")?;
            let name = normalized
                .file_name()
                .and_then(|s| s.to_str())
                .context("Invalid memory filename")?;
            ensure!(
                name == "MEMORY.md" || super::super::memory::files::valid_filename(name),
                "Invalid memory filename"
            );
            managed = Some(if directory
                .canonicalize()
                .ok()
                .zip(self.directory.canonicalize().ok())
                .is_some_and(|(left, right)| left == right)
                || directory == &self.directory
            {
                name.to_owned()
            } else {
                normalized
                    .to_str()
                    .context("Invalid memory path")?
                    .to_owned()
            });
        }
        if managed.is_some() {
            ensure!(
                paths.len() == 1,
                "Memory patches must affect exactly one file; mixed patches are refused"
            );
            ensure!(
                matches!(tool, "Read" | "Write" | "Edit" | "ApplyPatch"),
                "Use Read/Write/Edit or an exact ApplyPatch deletion for memory"
            );
            if tool == "ApplyPatch" {
                let lines = input["patch"]
                    .as_str()
                    .unwrap_or_default()
                    .lines()
                    .collect::<Vec<_>>();
                ensure!(
                    lines.len() == 3
                        && lines[0] == "*** Begin Patch"
                        && lines[1].starts_with("*** Delete File: ")
                        && lines[2] == "*** End Patch",
                    "Memory ApplyPatch supports one exact Delete File only"
                );
                ensure!(
                    managed
                        .as_deref()
                        .and_then(|name| Path::new(name).file_name())
                        .is_none_or(|name| name != "MEMORY.md"),
                    "Delete a memory file, not its index"
                );
            }
        }
        Ok(managed)
    }

    pub fn tool(
        &self,
        tool: &str,
        input: &Value,
        filename: &str,
        read_only: bool,
    ) -> Result<Value> {
        ensure!(self.active(), "File memory is disabled");
        ensure!(
            tool == "Read" || !read_only,
            "Plan mode: memory is read-only"
        );
        let session = self
            .session
            .as_ref()
            .context("Model memory tools require a session")?;
        let _guard = session.hold_incarnation()?;
        let target = self.directory.join(filename);
        let filename = target
            .file_name()
            .and_then(|n| n.to_str())
            .context("Missing memory filename")?;
        ensure!(
            filename == "MEMORY.md" || crate::memory::files::valid_filename(filename),
            "Invalid memory filename"
        );
        let directories = self.session_directories()?;
        let (scope, directory) = directories
            .iter()
            .find(|(_, dir)| {
                target.parent().is_some_and(|parent| {
                    parent
                        .canonicalize()
                        .ok()
                        .zip(dir.canonicalize().ok())
                        .is_some_and(|(left, right)| left == right)
                        || parent == dir.as_path()
                })
            })
            .context("Memory scope changed")?;
        let data = self.context.paths.data_dir.canonicalize()?;
        let root = if *scope == "team" { directory } else { &data };
        let is_index = filename == "MEMORY.md";
        let current = Self::from_session(&self.context, session.clone(), None)?;
        ensure!(
            self.scope == current.scope,
            "Memory scope changed during the turn"
        );
        let store = if tool == "Read" {
            None
        } else if *scope == "project" {
            Some(self.store(false)?)
        } else {
            Some(FileMemoryStore::open_resident(root, directory, false)?)
        };
        let before = FileMemoryStore::read_resident_file(root, directory, filename)?;
        let receipt_key = format!(
            "read.{}.{}",
            crate::memory::files::content_hash(directory.as_os_str().as_encoded_bytes()),
            filename
        );
        if tool == "Read" {
            let bytes = before.context("Memory file not found")?;
            let text = std::str::from_utf8(&bytes)?;
            let offset = input["offset"].as_u64().unwrap_or(1).max(1) as usize;
            let count = input["limit"].as_u64().unwrap_or(usize::MAX as u64) as usize;
            let lines = text.lines().collect::<Vec<_>>();
            let shown = lines
                .iter()
                .enumerate()
                .skip(offset - 1)
                .take(count)
                .map(|(i, line)| format!("{}\t{}", i + 1, line))
                .collect::<Vec<_>>()
                .join("\n");
            let hash = crate::memory::files::content_hash(&bytes);
            if offset == 1 && count >= lines.len() {
                session.write(|w| {
                    w.set_setting(
                        settings::Setting::memory_receipt(receipt_key.clone()),
                        &hash,
                    )
                })?;
            }
            return Ok(json!({"success":true,"content":shown,"file_path":target,"version":hash}));
        }
        let store = store.context("Missing writable memory store")?;
        let expected = match &before {
            None => ExpectedVersion::Absent,
            Some(bytes) => {
                let seen = session
                    .read(|r| {
                        r.setting::<String>(settings::Setting::memory_receipt(receipt_key.clone()))
                    })?
                    .context("Read the complete memory file before modifying it")?;
                ensure!(
                    seen == crate::memory::files::content_hash(bytes),
                    "Memory changed since Read; read it again (Conflict)"
                );
                ExpectedVersion::Hash(seen)
            }
        };
        if tool == "ApplyPatch" {
            store.delete(filename, expected)?;
            return Ok(json!({"success":true,"deleted":filename}));
        }
        let text = if tool == "Write" {
            input["content"]
                .as_str()
                .context("content required")?
                .to_owned()
        } else {
            let original = String::from_utf8(before.context("Memory file not found")?)?;
            let old = input["old_string"]
                .as_str()
                .context("old_string required")?;
            let new = input["new_string"]
                .as_str()
                .context("new_string required")?;
            ensure!(!old.is_empty(), "old_string must be nonempty");
            let count = original.matches(old).count();
            ensure!(
                count > 0 && (count == 1 || input["replace_all"] == true),
                "old_string must match exactly and uniquely"
            );
            if input["replace_all"] == true {
                original.replace(old, new)
            } else {
                original.replacen(old, new, 1)
            }
        };
        let version = if is_index {
            store.write_resident_index(text.as_bytes(), expected)?
        } else {
            store.put(filename, text.as_bytes(), expected)?.hash
        };
        session.write(|w| {
            w.set_setting(
                settings::Setting::memory_receipt(receipt_key.clone()),
                &version,
            )
        })?;
        if is_index {
            return Ok(json!({"success":true,"saved":filename,"version":version}));
        }
        let snapshot = store.snapshot()?;
        let index = snapshot.index_context(&self.directory.join("MEMORY.md").to_string_lossy());
        Ok(
            json!({"success":true,"saved":filename,"version":version,"index_over_budget":index.truncated,"omitted":index.omitted}),
        )
    }
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn relevance(document: &crate::memory::files::MemoryDocument, query: &str) -> f64 {
    let terms = crate::memory::search_terms(&query.to_lowercase());
    if terms.is_empty() {
        return 0.0;
    }
    let ratio = |field: &str| {
        let lower = field.to_lowercase();
        terms
            .iter()
            .filter(|term| lower.contains(term.as_str()))
            .count() as f64
            / terms.len() as f64
    };
    0.25 * ratio(&document.name) + 0.5 * ratio(&document.description) + 0.25 * ratio(&document.body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    fn fixture() -> (tempfile::TempDir, FileMemory) {
        let root = tempfile::tempdir().unwrap();
        let paths = crate::runtime::RuntimePaths::new(root.path().join("data")).unwrap();
        let context =
            ServiceContext::open(paths, Arc::new(super::super::EnvironmentCredentials)).unwrap();
        let workspace = root.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        crate::runtime::SessionStore::new(context.db.clone())
            .create_with_id(&workspace, "fixture", "fixture", &id)
            .unwrap();
        let memory = FileMemory::session(&context, &id).unwrap();
        (root, memory)
    }
    fn note(body: &str) -> String {
        format!("---\nname: 中文 preferences\ndescription: Rust testing 中文测试 preferences\nmetadata:\n  type: user\n---\n{body}\n")
    }
    #[test]
    fn file_memory_imports_legacy_configuration_only_once() {
        let root = tempfile::tempdir().unwrap();
        let paths = crate::runtime::RuntimePaths::new(root.path().join("data")).unwrap();
        let context =
            ServiceContext::open(paths, Arc::new(super::super::EnvironmentCredentials)).unwrap();
        let archive = context.paths.data_dir.join("data/memory.sqlite3");
        let conn = rusqlite::Connection::open(&archive).unwrap();
        conn.execute_batch(
            r#"CREATE TABLE memory_settings(id INTEGER, config TEXT);
            INSERT INTO memory_settings VALUES(1,'{"enabled":false,"search_top_k":3}')"#,
        )
        .unwrap();
        assert!(!config(&context).unwrap().enabled);
        drop(conn);
        std::fs::remove_file(&archive).unwrap();
        assert_eq!(config(&context).unwrap().search_top_k, 3);
        save_config(&context, &Default::default()).unwrap();
        assert!(config(&context).unwrap().enabled);
        assert!(!archive.exists());
    }

    #[test]
    fn file_memory_cas_tools() {
        let (_root, memory) = fixture();
        let path = memory.directory.join("test.md");
        memory
            .tool(
                "Write",
                &json!({"content":note("Prefer isolated tests")}),
                "test.md",
                false,
            )
            .unwrap();
        assert_eq!(
            memory
                .target("Read", &json!({"file_path":path}), "/")
                .unwrap(),
            Some("test.md".into())
        );
        memory.tool("Read", &json!({}), "test.md", false).unwrap();
        let snapshot = memory.listing("").unwrap();
        let version = snapshot["items"][0]["version"].as_str().unwrap();
        memory
            .put(
                "test.md",
                &note("Another session wrote this"),
                Some(version),
            )
            .unwrap();
        assert!(memory
            .tool(
                "Edit",
                &json!({"old_string":"Another session", "new_string":"Current session"}),
                "test.md",
                false
            )
            .unwrap_err()
            .to_string()
            .contains("Conflict"));
        memory.tool("Read", &json!({}), "test.md", false).unwrap();
        memory
            .tool(
                "Edit",
                &json!({"old_string":"Another session", "new_string":"Current session"}),
                "test.md",
                false,
            )
            .unwrap();
        assert!(memory
            .store(false)
            .unwrap()
            .read("test.md")
            .unwrap()
            .unwrap()
            .document
            .body
            .contains("Current session"));
        memory
            .tool("ApplyPatch", &json!({}), "test.md", false)
            .unwrap();
        assert!(memory.listing("").unwrap()["items"]
            .as_array()
            .unwrap()
            .is_empty());
    }
    #[test]
    fn memory_oversized_disk_index_truncates_before_escaping_and_hashes_tail() {
        let (_root, memory) = fixture();
        memory.store(false).unwrap();
        let path = memory.directory.join("MEMORY.md");
        let shard = memory.session.as_ref().unwrap();
        std::fs::write(&path, "&".repeat(6000) + "TAIL-MARKER").unwrap();
        shard.admit_prompt("small", None, "typed").unwrap();
        let small = replay(shard).0.unwrap();
        assert_eq!(small.matches("&amp;").count(), 6000);
        assert!(small.contains("TAIL-MARKER"));
        let raw = "<".repeat(9 * 1024 * 1024);
        std::fs::write(&path, &raw).unwrap();
        shard.admit_prompt("large", None, "typed").unwrap();
        prompt::Admission::refresh(shard).unwrap();
        let block = &replay(shard).1[0].blocks[0];
        assert!(block.contains("[MEMORY.md truncated") && block.matches("&lt;").count() > 25000);
        std::fs::write(path, raw + "x").unwrap();
        shard.admit_prompt("tail changed", None, "typed").unwrap();
        prompt::Admission::refresh(shard).unwrap();
        assert_eq!(replay(shard).1.len(), 2);
    }

    fn replay(shard: &SessionDb) -> (Option<String>, Vec<crate::services::turn_context::Entry>) {
        let (history, boundary) = shard
            .read(|r| r.model_history(&crate::native_engine::provider_manager::ApiFormat::OpenAI))
            .unwrap();
        let (indexes, entries) = prompt::replay(shard, boundary.as_deref(), &history).unwrap();
        ((!indexes.is_empty()).then(|| indexes.join("\n")), entries)
    }

    #[test]
    fn memory_sync_boundary_freezes_the_current_scope() {
        let (_root, memory) = fixture();
        memory.store(false).unwrap();
        let path = memory.directory.join("MEMORY.md");
        let shard = memory.session.as_ref().unwrap();
        std::fs::write(&path, "SESSION_INDEX").unwrap();
        shard.admit_prompt("initial", None, "typed").unwrap();
        let mut row = shard.read(|r| r.messages()).unwrap().remove(0);
        for promote in [false, true] {
            row.id = uuid::Uuid::new_v4().to_string();
            row.sort_order += 1;
            row.content = format!("sync {promote}");
            let sync = |row| {
                memory
                    .context
                    .db
                    .sync_conversation(shard.id(), None, &[row])
                    .unwrap()
            };
            if promote {
                row.is_compact_boundary = false;
                sync(row.clone());
            }
            let index = format!("SYNC_INDEX_{promote}");
            std::fs::write(&path, &index).unwrap();
            row.is_compact_boundary = true;
            sync(row.clone());
            assert!(replay(shard).0.unwrap_or_default().contains(&index));
            std::fs::write(&path, "UNADMITTED_EDIT").unwrap();
            row.content.push('!');
            sync(row.clone());
            assert!(replay(shard).0.unwrap_or_default().contains(&index));
        }
    }

    fn compact(shard: &SessionDb) {
        let prepared = crate::services::compaction::prepare(shard, shard.id()).unwrap();
        crate::services::compaction::commit(shard, prepared, "same summary").unwrap();
    }

    #[test]
    fn memory_first_chat_uses_the_producer_origin_for_initial_and_later_submissions() {
        let (_root, memory) = fixture();
        let stores = &memory.context.db;
        let id = "generated-first-chat";
        let content = json!("generated directive");
        let (lease, sealed) = stores
            .admit_chat_execution(id, "model", Some(&content), "delegation")
            .unwrap();
        assert!(sealed);
        drop(lease);
        stores
            .admit_chat_message(id, "model", Some(&content), "mcp_prompt")
            .unwrap();
        let shard = stores.open_session(id).unwrap();
        shard
            .read(|r| {
                assert!(r.authority()?.is_empty());
                let messages = r.messages()?;
                assert_eq!(messages.len(), 2);
                assert_eq!(
                    r.message_metadata(&messages[0].id)?.unwrap()["promptOrigin"],
                    "delegation"
                );
                assert_eq!(
                    r.message_metadata(&messages[1].id)?.unwrap()["promptOrigin"],
                    "mcp_prompt"
                );
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn memory_claimed_boundary_freezes_its_own_index_and_origin() {
        let (_root, memory) = fixture();
        let shard = memory.session.as_ref().unwrap();
        memory.store(false).unwrap();
        let index = memory.directory.join("MEMORY.md");
        std::fs::write(&index, "CLAIMED-INDEX").unwrap();
        shard
            .write(|w| {
                w.append("user", "question")?;
                w.append("assistant", "answer")?;
                w.request_compaction("retain", 50, 100)
            })
            .unwrap();
        let owner = shard.delivery_owner().unwrap();
        let claim = owner.claim_compaction().unwrap().unwrap();
        let id = owner.finish_compaction(&claim, &"finished".into()).unwrap();
        drop(owner);
        std::fs::write(index, "later").unwrap();
        assert_eq!(
            shard.read(|r| r.message_metadata(&id)).unwrap().unwrap()["promptOrigin"],
            "compaction"
        );
        assert!(replay(shard).0.unwrap().contains("CLAIMED-INDEX"));
    }

    #[test]
    fn memory_partial_boundary_supersedes_copied_indexes() {
        use crate::services::compaction::{commit_part, Part};
        let (_root, memory) = fixture();
        let shard = memory.session.as_ref().unwrap();
        memory.put("note.md", &note("retained fact"), None).unwrap();
        let index = memory.directory.join("MEMORY.md");
        std::fs::write(&index, "INDEX-A").unwrap();
        shard.admit_prompt("first", None, "typed").unwrap();
        std::fs::write(&index, "INDEX-B").unwrap();
        let kept = shard.admit_prompt("preferences", None, "typed").unwrap();
        prompt::Admission::refresh(shard).unwrap();
        let prepared = shard.prepare_summary_part(&kept, Part::UpTo).unwrap();
        std::fs::write(&index, "INDEX-C").unwrap();
        let (branch, _) = commit_part(
            &memory.context.db,
            shard.id(),
            &kept,
            Part::UpTo,
            prepared,
            "summary",
        )
        .unwrap();
        let branch = memory.context.db.open_session(&branch).unwrap();
        let (prefix, entries) = replay(&branch);
        assert!(prefix.unwrap().contains("INDEX-C"));
        assert!(entries.is_empty());
        branch.admit_prompt("preferences", None, "typed").unwrap();
        assert!(replay(&branch).1.is_empty());
        assert!(replay(shard)
            .1
            .last()
            .unwrap()
            .blocks
            .join("")
            .contains("INDEX-B"));
    }

    #[test]
    fn memory_text_boundary_rebuilds_the_current_scope_prefix() {
        let (root, first) = fixture();
        let workspace = root.path().join("workspace");
        first.store(false).unwrap();
        std::fs::write(first.directory.join("MEMORY.md"), "SCOPE-A").unwrap();
        let id = crate::runtime::SessionStore::new(first.context.db.clone())
            .create(&workspace, "fixture", "fixture")
            .unwrap();
        let shard = first.context.db.open_session(&id).unwrap();
        shard.admit_prompt("hello", None, "typed").unwrap();
        let second_root = root.path().join("other");
        std::fs::create_dir(&second_root).unwrap();
        crate::services::session_workspace::change(
            &first.context.db,
            &id,
            &workspace,
            second_root.to_str().unwrap(),
            true,
        )
        .unwrap();
        let second = FileMemory::session(&first.context, &id).unwrap();
        assert_ne!(first.scope.key(), second.scope.key());
        second.store(false).unwrap();
        std::fs::write(second.directory.join("MEMORY.md"), "SCOPE-B").unwrap();
        shard.admit_prompt("next", None, "typed").unwrap();
        prompt::Admission::refresh(&shard).unwrap();
        let (prefix, entries) = replay(&shard);
        assert!(prefix.unwrap().contains("SCOPE-A"));
        assert!(entries[0].blocks[0].contains("SCOPE-B"));
        compact(&shard);
        std::fs::write(second.directory.join("MEMORY.md"), "later edit").unwrap();
        let (prefix, turns) = replay(&shard);
        let prefix = prefix.unwrap();
        assert!(prefix.contains("SCOPE-B") && !prefix.contains("SCOPE-A"));
        assert!(turns.is_empty());
    }

    #[test]
    fn file_memory_plan_never_creates_repairs_or_writes() {
        let (_root, memory) = fixture();
        prompt::Admission::session(memory.session.as_ref().unwrap())
            .unwrap()
            .frozen_index()
            .unwrap();
        assert!(!memory.directory.exists());
        memory.put("test.md", &note("Fact"), None).unwrap();
        std::fs::write(memory.directory.join("MEMORY.md"), "broken index\n").unwrap();
        let before = std::fs::read(memory.directory.join("MEMORY.md")).unwrap();
        let index = prompt::Admission::session(memory.session.as_ref().unwrap())
            .unwrap()
            .frozen_index()
            .unwrap()
            .render()
            .join("\n");
        assert!(index.contains("broken index"));
        assert_eq!(
            std::fs::read(memory.directory.join("MEMORY.md")).unwrap(),
            before
        );
        assert!(memory
            .tool(
                "Write",
                &json!({"content":note("Changed")}),
                "test.md",
                true
            )
            .is_err());
        memory.tool("Read", &json!({}), "test.md", true).unwrap();
    }
    #[test]
    fn file_memory_target_rejects_other_scopes_traversal_and_mixed_patches() {
        let (_root, memory) = fixture();
        let directory = memory.directory.to_string_lossy();
        let sep = std::path::MAIN_SEPARATOR;
        for path in [
            PathBuf::from(format!("{directory}{sep}..{sep}memory{sep}test.md")),
            PathBuf::from(format!(
                "{directory}{sep}..{sep}..{sep}other{sep}memory{sep}test.md"
            )),
            memory
                .context
                .paths
                .data_dir
                .join("data/memory/state/catalog.json"),
        ] {
            assert!(
                memory
                    .target("Write", &json!({"file_path":path}), "/")
                    .is_err(),
                "{path:?}"
            );
        }
        let path = memory.directory.join("test.md");
        assert!(memory.target("ApplyPatch",&json!({"patch":format!("*** Begin Patch\n*** Delete File: {}\n*** Delete File: outside.txt\n*** End Patch",path.display())}),"/").is_err());
        assert!(memory.target("ApplyPatch",&json!({"patch":format!("*** Begin Patch\n*** Delete File: {}\n*** End Patch",path.display())}),"/").unwrap().is_some());
        assert!(memory
            .target("Write", &json!({"file_path":"ordinary.md"}), "/tmp")
            .unwrap()
            .is_none());
    }
    #[test]
    fn file_memory_scope_selection_and_restart_binding() {
        let (_root, memory) = fixture();
        assert!(FileMemory::select(&memory.context, &Selection::default()).is_err());
        let again =
            FileMemory::session(&memory.context, memory.session.as_ref().unwrap().id()).unwrap();
        assert_eq!(again.scope.key(), memory.scope.key());
        memory
            .session
            .as_ref()
            .unwrap()
            .write(|w| {
                let (project, _) = w.setting(settings::MEMORY_SCOPE)?.unwrap();
                w.set_setting(settings::MEMORY_SCOPE, &(project, "other".into()))
            })
            .unwrap();
        assert!(
            FileMemory::session(&memory.context, memory.session.as_ref().unwrap().id()).is_err()
        );
        assert!(memory
            .context
            .global_setting::<bool>(GlobalSetting::MachinePreference("memory.files.active"))
            .unwrap()
            .unwrap());
    }
    #[test]
    fn file_memory_sessions_share_git_worktrees_and_rebind_explicit_multi_root() {
        let (root, fixture) = fixture();
        let repo = root.path().join("repo");
        let tree = root.path().join("linked");
        std::fs::create_dir_all(repo.join(".git/objects")).unwrap();
        std::fs::create_dir_all(repo.join(".git/refs/heads")).unwrap();
        std::fs::create_dir_all(repo.join("src")).unwrap();
        std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir_all(&tree).unwrap();
        let metadata = repo.join(".git/worktrees/linked");
        std::fs::create_dir_all(&metadata).unwrap();
        std::fs::write(metadata.join("HEAD"), "ref: refs/heads/linked\n").unwrap();
        std::fs::write(metadata.join("commondir"), "../..\n").unwrap();
        std::fs::write(
            metadata.join("gitdir"),
            format!("{}\n", tree.join(".git").display()),
        )
        .unwrap();
        std::fs::write(
            tree.join(".git"),
            format!("gitdir: {}\n", metadata.display()),
        )
        .unwrap();
        let sessions = crate::runtime::SessionStore::new(fixture.context.db.clone());
        let first_id = sessions.create(&repo, "fixture", "fixture").unwrap();
        let first = FileMemory::session(&fixture.context, &first_id).unwrap();
        first
            .put("test.md", &note("Shared across worktrees"), None)
            .unwrap();
        for path in [repo.join("src"), tree] {
            let id = sessions.create(&path, "fixture", "fixture").unwrap();
            let second = FileMemory::session(&fixture.context, &id).unwrap();
            assert_eq!(first.scope, second.scope);
            assert_eq!(
                second.listing("").unwrap()["items"]
                    .as_array()
                    .unwrap()
                    .len(),
                1
            );
        }
        let other = root.path().join("extra");
        std::fs::create_dir(&other).unwrap();
        crate::services::session_workspace::change(
            &fixture.context.db,
            &first_id,
            &repo,
            other.to_str().unwrap(),
            true,
        )
        .unwrap();
        let rebound = FileMemory::session(&fixture.context, &first_id).unwrap();
        assert_ne!(first.scope, rebound.scope);
        assert!(first.store(false).is_err());
        assert!(rebound.listing("").unwrap()["items"]
            .as_array()
            .unwrap()
            .is_empty());
    }

    #[test]
    fn file_memory_home_is_private_and_owned_children_share_it() {
        let (_root, fixture) = fixture();
        let home = dirs::home_dir().unwrap();
        // Only the temporary database is written; Home is an existing root.
        let sessions = crate::runtime::SessionStore::new(fixture.context.db.clone());
        let parent_id = sessions.create(&home, "fixture", "fixture").unwrap();
        let other_id = sessions.create(&home, "fixture", "fixture").unwrap();
        let parent = FileMemory::session(&fixture.context, &parent_id).unwrap();
        let other = FileMemory::session(&fixture.context, &other_id).unwrap();
        assert!(parent.scope.is_private());
        assert_ne!(parent.scope, other.scope);
        let child_id = sessions.create(&home, "fixture", "fixture").unwrap();
        let child = parent.child(&child_id).unwrap();
        assert_eq!(parent.scope, child.scope);
        assert_eq!(
            FileMemory::session(&fixture.context, &child_id)
                .unwrap()
                .scope,
            parent.scope
        );
        assert!(FileMemory::workspace(&fixture.context, &home).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn file_memory_tools_reject_links() {
        let (root, memory) = fixture();
        memory.store(false).unwrap();
        let external = root.path().join("outside.md");
        std::fs::write(&external, note("outside")).unwrap();
        std::os::unix::fs::symlink(&external, memory.directory.join("test.md")).unwrap();
        let alias = root.path().join("alias");
        std::os::unix::fs::symlink(&memory.directory, &alias).unwrap();
        assert!(memory
            .target("Write", &json!({"file_path":alias.join("new.md")}), "/")
            .is_err());
        assert!(memory.tool("Read", &json!({}), "test.md", false).is_err());
        assert!(memory
            .tool(
                "Write",
                &json!({"content":note("changed")}),
                "test.md",
                false
            )
            .is_err());
        assert!(std::fs::read_to_string(external)
            .unwrap()
            .contains("outside"));
    }
    #[test]
    fn memory_sync_tied_boundary_supersedes_index() {
        let (_root, memory) = fixture();
        memory.store(false).unwrap();
        let path = memory.directory.join("MEMORY.md");
        let shard = memory.session.as_ref().unwrap();
        std::fs::write(&path, "EARLIER_A").unwrap();
        shard.admit_prompt("old", None, "typed").unwrap();
        std::fs::write(&path, "OUTDATED_B").unwrap();
        shard.admit_prompt("retained", None, "typed").unwrap();
        prompt::Admission::refresh(shard).unwrap();
        let mut rows = shard.repository().messages().unwrap();
        let mut boundary = rows.remove(0);
        boundary.is_compact_boundary = true;
        boundary.sort_order = rows[0].sort_order;
        std::fs::write(&path, "FROZEN_C").unwrap();
        memory
            .context
            .db
            .sync_conversation(shard.id(), None, &[boundary])
            .unwrap();
        let (prefix, entries) = replay(shard);
        assert!(
            prefix.unwrap().contains("FROZEN_C")
                && !json!(entries).to_string().contains("OUTDATED_B")
        );
    }

    #[test]
    fn memory_sync_demotion_restores_index() {
        let (_root, mem) = fixture();
        mem.store(false).unwrap();
        let shard = mem.session.as_ref().unwrap();
        let path = mem.directory.join("MEMORY.md");
        std::fs::write(&path, "ORIGINAL_FROZEN_A").unwrap();
        shard.admit_prompt("prior", None, "typed").unwrap();
        let mut row = shard.read(|r| r.messages()).unwrap()[0].clone();
        row.id = uuid::Uuid::new_v4().to_string();
        row.sort_order += 1;
        row.content = "sync marker".into();
        let sync = |m| {
            mem.context
                .db
                .sync_conversation(shard.id(), None, &[m])
                .unwrap()
        };
        sync(row.clone());
        std::fs::write(&path, "RETAINED_B").unwrap();
        shard.admit_prompt("receives B", None, "typed").unwrap();
        prompt::Admission::refresh(shard).unwrap();
        let (original, entries) = replay(shard);
        assert!(json!(entries).to_string().contains("RETAINED_B"));
        std::fs::write(&path, "TEMPORARY_C").unwrap();
        row.is_compact_boundary = true;
        sync(row.clone());
        assert!(replay(shard).0.unwrap().contains("TEMPORARY_C"));
        row.is_compact_boundary = false;
        // The changed sync payload is a fresh digest, so it exercises demotion.
        row.content.push_str(" demoted");
        sync(row.clone());
        let (after, saved) = replay(shard);
        assert_eq!(after, original);
        assert!(json!(saved).to_string().contains("RETAINED_B"));
    }
    #[test]
    fn memory_sync_interruption_preserves_scope() -> Result<()> {
        use crate::db::session_store::{Phase, Stores, SyncHeader};
        let (root, mem_a) = fixture();
        let shard = mem_a.session.as_ref().unwrap();
        let stores = &mem_a.context.db;
        let ws_b = root.path().join("other-project");
        std::fs::create_dir(&ws_b)?;
        let b = stores.create_code_session(&ws_b, "m", "p", &uuid::Uuid::new_v4().to_string())?;
        let mem_b = FileMemory::session(&mem_a.context, b.id())?;
        mem_b.store(false)?;
        std::fs::write(
            mem_b.directory.join("MEMORY.md"),
            "OTHER_PROJECT_INDEX_69C4",
        )?;
        shard.admit_prompt("old in A", None, "typed")?;
        let mut row = shard.read(|r| r.messages())?[0].clone();
        row.id = uuid::Uuid::new_v4().to_string();
        row.sort_order += 1;
        row.content = "received summary".into();
        row.is_compact_boundary = true;
        let a_project = stores
            .global()
            .conversation(shard.id())?
            .unwrap()
            .project_id
            .unwrap();
        let b_project = stores
            .global()
            .conversation(b.id())?
            .unwrap()
            .project_id
            .unwrap();
        assert_ne!(a_project, b_project);
        let header: SyncHeader = serde_json::from_value(
            json!({"id":shard.id(),"projectId":b_project,"createdAt":"2026-01-01T00:00:00Z","updatedAt":"2099-01-01T00:00:00Z"}),
        )?;
        let fault = stores.with_fault_hook(|p| {
            anyhow::ensure!(
                p != Phase::SyncContent,
                "interrupted before scope publication"
            );
            Ok(())
        });
        let err = fault
            .sync_conversation(shard.id(), Some(&header), &[row])
            .unwrap_err();
        assert!(
            err.chain()
                .any(|cause| cause.to_string() == "interrupted before scope publication"),
            "{err:#}"
        );
        std::fs::write(mem_b.directory.join("MEMORY.md"), "EDIT_AFTER_INTERRUPTION")?;
        let reopened = Stores::open(&mem_a.context.paths.data_dir)?;
        let live = reopened.open_session(shard.id())?;
        let current = reopened
            .global()
            .conversation(live.id())?
            .unwrap()
            .project_id
            .unwrap();
        assert!(current == a_project || current == b_project);
        let (prefix, _) = replay(&live);
        assert_eq!(
            prefix.unwrap().contains("OTHER_PROJECT_INDEX_69C4"),
            current == b_project
        );
        Ok(())
    }
}
