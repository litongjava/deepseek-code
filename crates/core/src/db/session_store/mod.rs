//! Process-local resolver for the global directory and incarnation-bound session shards.
mod approvals;
mod access;
pub use access::{H5Session,HostAccess};
mod agents;
mod plugins;
pub use plugins::{PluginCatalog,PluginInstall};
pub use agents::AgentCatalog;
mod admission;
mod binding;
pub(crate) mod boundary;
mod compaction;
pub(crate) use compaction::CompactionWindow;
mod directory;
pub mod diagnostics;
mod conversations;
mod conversation_edit;
pub use conversations::Rewind;
pub use compaction::{CheckpointKey, CompactionClaim};
pub use directory::{DirectoryEntry, DirectoryFilter, ResumeEntry};
mod delivery;
mod desktop_queue;
mod executor;
mod export;
mod engine;
mod children;
mod child_reader;
mod worker_directory;
mod peers;

pub use worker_directory::{WorkerDirectory, WorkerPair};
mod recording;
pub use recording::RecordingPolicies;
mod segment;
mod owned;
mod observation;
mod goal_state;
pub use owned::{SessionAccess,SessionRepository};
pub use segment::{SegmentPlacement, SegmentUpdate};
pub use executor::{ExecutorHistory,HookSessions};
mod global;
mod shared;
mod leases;
pub use leases::ServiceLeases;
mod schedules;
pub use schedules::Schedules;
mod mcp_control;
pub use mcp_control::McpControl;
mod global_format;
mod history;

mod im;
pub use im::FeishuChatMapping;
mod im_catalog;
mod learning;
pub use learning::LearningRecords;
pub use im_catalog::ImCatalog;
mod im_permissions;
pub use im_permissions::{ImPermissionChange, ImPermissions};
pub mod magic;
mod documents;
mod team;
pub use team::TeamCatalog;
pub use documents::{DocumentCatalog,DocumentImport};
mod projects;
mod workspace_lock;
mod worktrees;
pub use projects::{ProjectEdit,Projects};
mod repair;
mod runtime;
pub mod settings;
mod state;
pub(crate) use state::message_origin;
pub(crate) mod suggestions;
mod usage;
mod task;
mod task_state;
mod task_roles;
pub use task_roles::TaskMembership;
pub use task_state::TaskState;
pub use delivery::*;
pub use history::*;
pub use state::{GlobalPreferences, GlobalSetting};
pub use usage::{CorpusUsage, DailyUsage};
mod layout;
pub(crate) use layout::without_lifecycle_wait;
mod lifecycle;
pub use global::GlobalDb;
pub use lifecycle::{
    Branch, CreateOptions, FirstMessage, ImportResult, Phase, ProjectSelection, SyncHeader,
};

use anyhow::{ensure, Context, Result};
use layout::{lock_keys, storage_key, Permits, SESSION};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::{Duration, Instant};

pub(crate) const WRITE_DEADLINE: Duration = Duration::from_secs(30);

type Fault = dyn Fn(Phase) -> Result<()> + Send + Sync;
struct Root {
    path: PathBuf,
    global: GlobalDb,
    host: Mutex<Weak<Stores>>,
    repair: std::sync::OnceLock<std::sync::mpsc::Sender<()>>,
    repair_cursor: Mutex<String>,
    recording: std::sync::RwLock<HashMap<String,crate::native_engine::request_recording::Policy>>,
    upgraded_directory: bool,
    migrating: std::sync::atomic::AtomicBool,
}
type RootSlot = Arc<Mutex<Weak<Root>>>;
static ROOTS: LazyLock<Mutex<HashMap<PathBuf, RootSlot>>> = LazyLock::new(Default::default);

impl Root {
    fn layout(&self) -> crate::data_layout::DataLayout {
        crate::data_layout::DataLayout::new(self.path.clone())
    }
}

#[derive(Clone)]
pub struct Stores {
    root: Arc<Root>,
    fault: Option<Arc<Fault>>,
    selected: Option<HistoryAddress>,
    router: Option<HistoryAddress>,
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum OpenMode {
    Host,
    Directory,
}

impl Stores {
    pub fn open(path: &Path) -> Result<Self> {
        layout::allow_entry()?;
        std::fs::create_dir_all(path)?;
        Self::open_existing(path, OpenMode::Host)
    }

    /// Inspect the published directory from a short-lived listing process.
    /// Session recovery and the projection worker start when a host opens the root.
    pub(crate) fn list_published(
        path: &Path,
    ) -> Result<Vec<crate::db::conversation_repo::ConversationRow>> {
        layout::allow_entry()?;
        std::fs::create_dir_all(path)?;
        Self::open_existing(path, OpenMode::Directory)?.conversations()
    }

    fn open_existing(path: &Path, mode: OpenMode) -> Result<Self> {
        layout::allow_entry()?;
        let outcome = crate::data_layout::DataLayout::from_root(&path).migrate_if_needed()?;
        if !outcome.warnings.is_empty() {
            tracing::warn!(
                module = "SessionStore",
                root = %path.display(),
                entries = %outcome.warnings.join(", "),
                "Legacy or unknown entries remain in the data directory root"
            );
        }
        ensure!(
            path.is_dir(),
            "Session store requires an existing data root"
        );
        let path = path.canonicalize()?;
        let slot = {
            let mut roots = ROOTS.lock().unwrap();
            roots.retain(|_, slot| {
                Arc::strong_count(slot) > 1 || slot.lock().unwrap().strong_count() > 0
            });
            roots.entry(path.clone()).or_default().clone()
        };
        // Opening or migrating one data root must never block another root.
        let mut cached = slot.lock().unwrap();
        let root = if let Some(existing) = cached.upgrade() {
            existing
        } else {
            let (uuid, upgraded_directory) = layout::open_root(&path)?;
            let mut global = GlobalDb::new(&path, uuid)?;
            Arc::new_cyclic(|weak| {
                global.root = weak.clone();
                Root {
                    global,
                    host: Default::default(),
                    upgraded_directory,
                    repair: Default::default(),
                    repair_cursor: Default::default(),
                    recording: Default::default(),
                    migrating: Default::default(),
                    path,
                }
            })
        };
        let stores = Self { root, fault: None, selected: None, router: None };
        if stores.root.repair.get().is_none() {
            drop(stores.namespace()?);
            if mode != OpenMode::Directory {
                stores.reconcile()?;
                stores.start_projection_repair();
            }
        }
        *cached = Arc::downgrade(&stores.root);
        drop(cached);
        Ok(stores)
    }

    pub(crate) fn host_handle(self) -> Arc<Self> {
        let root=self.root.clone();
        let mut slot=root.host.lock().unwrap_or_else(|e|e.into_inner());
        if let Some(host)=slot.upgrade() { return host; }
        let host=Arc::new(self);
        *slot=Arc::downgrade(&host);
        host
    }

    /// Inspect or corrupt directory-owned records in a fixture.
    #[cfg(test)]
    pub(crate) fn fixture_directory<T>(&self, body: impl FnOnce(&Connection) -> T) -> Result<T> {
        self.global().connection(OpenFlags::SQLITE_OPEN_READ_WRITE, |c| Ok(body(c)))
    }

    #[cfg(test)]
    pub(crate) fn fixture_shard<T>(&self, id: &str, body: impl FnOnce(&Connection) -> T) -> Result<T> {
        let session = self.open_session(id)?;
        let _guard = self.key_lock(&session.key, false, false)?;
        session.check_directory()?;
        session.connection_under_lock(OpenFlags::SQLITE_OPEN_READ_WRITE, |c| {
            layout::callback(|| Ok(body(c)))
        })
    }

    #[cfg(test)]
    pub(crate) fn fixture_history_write<T>(&self,id:&str,mut body:impl FnMut(&Connection)->Result<T>)->Result<T>{
        self.open_session(id)?.write(|w|body(w.0))
    }

    pub fn data_dir(&self) -> &Path {
        &self.root.path
    }
    pub fn layout(&self) -> crate::data_layout::DataLayout {
        self.root.layout()
    }
    #[doc(hidden)]
    pub fn with_fault_hook(
        &self,
        fault: impl Fn(Phase) -> Result<()> + Send + Sync + 'static,
    ) -> Self {
        Self {
            root: self.root.clone(),
            fault: Some(Arc::new(fault)),
            selected: self.selected.clone(),
            router: self.router.clone(),
        }
    }
    fn step(&self, phase: Phase) -> Result<()> {
        if let Some(hook) = &self.fault {
            hook(phase)?;
        }
        Ok(())
    }
    pub fn global(&self) -> &GlobalDb {
        &self.root.global
    }
    fn sessions(&self) -> PathBuf {
        self.root.layout().sessions()
    }
    fn namespace(&self) -> Result<std::fs::File> {
        let dir = self.root.layout().lock_path("session");
        let guard = layout::lock_existing(&dir.join(".directory"), false)?;
        ensure!(
            std::fs::read_to_string(dir.join(".initialized"))? == self.global().uuid,
            "Lifecycle namespace identity changed"
        );
        Ok(guard)
    }

    fn has_directory_state(&self, key: &str) -> Result<bool> {
        self.global().read(|c| {
            Ok(c.query_row(
                "SELECT EXISTS(SELECT 1 FROM conversations WHERE storage_key=?1
            UNION ALL SELECT 1 FROM lifecycle WHERE storage_key=?1)",
                [key],
                |r| r.get(0),
            )?)
        })
    }

    fn key_lock(&self, key: &str, exclusive: bool, allocate: bool) -> Result<Vec<std::fs::File>> {
        self.try_key_lock(key, exclusive, allocate)?
            .context("Session unavailable")
    }

    fn try_key_lock(
        &self,
        key: &str,
        exclusive: bool,
        allocate: bool,
    ) -> Result<Option<Vec<std::fs::File>>> {
        let dir = self.root.layout().lock_path("session");
        let path = dir.join(key);
        let namespace = self.namespace()?;
        if !path.try_exists()? {
            let registered = self.has_directory_state(key)?;
            // Another creator may have installed the permanent key while we read G.
            if !path.try_exists()? {
                ensure!(!registered, "Missing lock for registered session");
                if !allocate {
                    // Linearize an unknown-key lookup here. The caller must not
                    // subsequently inspect a newly published G row without a key lock.
                    return Ok(None);
                }
                match std::fs::File::create_new(&path) {
                    Ok(file) => file.sync_all()?,
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(e.into()),
                }
            }
        }
        // Repeat the barrier on retry if allocation succeeded but fsync failed.
        if allocate {
            layout::sync_dir(&dir)?;
        }
        let pending = self.sessions().join(key).join(conversation_edit::PENDING);
        let mut guards = loop {
            let locks = lock_keys(&dir, vec![key], exclusive)?;
            if !pending.try_exists()? {
                break locks;
            }
            if exclusive {
                self.recover_conversation_edit(key, &pending)?;
                break locks;
            }
            drop(locks);
            let _writer = lock_keys(&dir, vec![key], true)?;
            self.recover_conversation_edit(key, &pending)?;
        };
        guards.push(namespace);
        Ok(Some(guards))
    }

    fn resolved_key(&self, key: &str) -> Result<Option<Vec<std::fs::File>>> {
        let Some(shared) = self.try_key_lock(key, false, false)? else {
            return Ok(None);
        };
        if self.pending(key)?.is_none() {
            return Ok(Some(shared));
        }
        drop(shared);
        let exclusive = self.group_access(vec![(key, true, false)])?;
        Ok(Some(exclusive))
    }

    /// Background job ownership is published in G. Checking that identity needs
    /// neither transcript access nor the session lifecycle lock.
    pub(crate) fn published_incarnation_matches(&self, id: &str, incarnation: &str) -> Result<bool> {
        Ok(self.global().incarnation(id, &storage_key(id)?)?.as_deref() == Some(incarnation))
    }

    pub fn open_session(&self, id: &str) -> Result<SessionDb> {
        let expected = self.selected.as_ref().filter(|a| a.session == id).map(|a| a.incarnation.as_str());
        self.open_current_session(id, expected)?
            .context("Session unavailable")
    }

    pub fn open_recorded(&self, id: &str, incarnation: &str) -> Result<SessionDb> {
        self.check_selected(id, incarnation)?;
        self.open_current_session(id, Some(incarnation))?
            .context("Session incarnation changed")
    }

    fn check_selected(&self, id: &str, incarnation: &str) -> Result<()> {
        ensure!(self.selected.as_ref().is_none_or(|a| a.session != id || a.incarnation == incarnation), "Selected session incarnation changed");
        Ok(())
    }

    fn open_current_session(&self, id: &str, expected: Option<&str>) -> Result<Option<SessionDb>> {
        let key = storage_key(id)?;
        let Some(mut locks) = self.resolved_key(&key)? else {
            return Ok(None);
        };
        let Some(incarnation) = self.global().incarnation(id, &key)? else {
            return Ok(None);
        };
        if expected.is_some_and(|expected| expected != incarnation) {
            return Ok(None);
        }
        let handle = SessionDb {
            stores: self.clone(),
            id: id.into(),
            key,
            incarnation,
        };
        let path = self.sessions().join(&handle.key).join("session.db");
        let inspect = || {
            handle.connection_under_lock(OpenFlags::SQLITE_OPEN_READ_ONLY, |c| {
                Ok(c.query_row(
                    "SELECT content_revision FROM manifest WHERE singleton=1",
                    [],
                    |row| row.get::<_, i64>(0),
                )?)
            })
        };
        let revision = match inspect() {
            Err(error) if error.is::<layout::SessionNeedsUpgrade>() => {
                drop(locks);
                // An enclosing operation may still hold a shared guard on this key.
                locks = layout::without_lifecycle_wait(|| self.key_lock(&handle.key, true, false))
                    .map_err(|error| {
                        if error.is::<layout::LifecycleBusy>() {
                            error.context(format!("Session {id} is busy; retry the format upgrade"))
                        } else {
                            error
                        }
                    })?;
                handle.check_directory()?;
                handle.upgrade_locked()?;
                inspect()?
            }
            result => result?,
        };
        let _locks = locks;
        drop(admission::open(&path)?);
        handle.try_project_current(revision);
        Ok(Some(handle))
    }
}

#[derive(PartialEq)]
struct VerifiedShard {
    file: (u64, u64),
    schema: i64,
    incarnation: String,
    external_edit: Option<[u8; 16]>,
}
// Verification outlives the last resolver handle, without retaining a root,
// pool or open file. Roots with different stores never share verification.
type VerificationSlot = Arc<Mutex<Option<VerifiedShard>>>;
static VERIFIED: LazyLock<Mutex<HashMap<(PathBuf, String), VerificationSlot>>> =
    LazyLock::new(Default::default);
impl Stores {
    fn verification_slot(&self, key: &str) -> VerificationSlot {
        VERIFIED
            .lock()
            .unwrap()
            .entry((self.root.path.clone(), key.into()))
            .or_default()
            .clone()
    }
    fn forget_verification(&self, key: &str) {
        VERIFIED
            .lock()
            .unwrap()
            .remove(&(self.root.path.clone(), key.into()));
    }
}

#[derive(Clone)]
pub struct SessionDb {
    stores: Stores,
    id: String,
    key: String,
    incarnation: String,
}

/// Keep SQLite attached to the WAL between the writes of one executing turn.
/// There is no open transaction. Dropping the last connection checkpoints normally.
pub(crate) struct TurnWal {
    _connection: Connection,
    _permit: Permits,
    _lease: Arc<crate::services::session_messages::ExecutionLease>,
}

impl SessionDb {
    pub fn memory_directory(&self) -> PathBuf {
        self.stores.sessions().join(&self.key).join("memory")
    }

    pub(crate) fn hold_turn_wal(
        &self,
        lease: Arc<crate::services::session_messages::ExecutionLease>,
    ) -> Result<Option<TurnWal>> {
        // At most four active turns retain a connection. Leave twelve of the
        // sixteen process-wide permits for concurrent transactions and forks.
        Permits::try_with_reserve(12)
            .map(|permit| self.open_turn_wal(lease, permit))
            .transpose()
    }

    fn open_turn_wal(
        &self,
        lease: Arc<crate::services::session_messages::ExecutionLease>,
        permit: Permits,
    ) -> Result<TurnWal> {
        ensure!(
            lease.address.as_ref() == Some(&self.address()),
            "Turn WAL requires the session's execution lease"
        );
        let path = self.stores.sessions().join(&self.key).join("session.db");
        let conn = layout::open(&path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        verify_manifest(
            &conn,
            self.stores.global().store_uuid(),
            &self.id,
            &self.incarnation,
        )?;
        Ok(TurnWal {
            _connection: conn,
            _permit: permit,
            _lease: lease,
        })
    }

    /// Preserve the initiating shard when a service also needs directory access.
    pub fn scoped_stores(&self) -> Stores {
        let mut stores = self.stores.clone();
        stores.selected = Some(self.address());
        stores
    }

    pub(crate) fn new_message_id(&self) -> String {
        crate::db::transcript::MessageIds::new(self.incarnation()).issue()
    }

    /// Keep the selected directory identity live while a host installs its local state.
    pub fn hold_incarnation(&self) -> Result<impl Drop> {
        let locks = self.stores.key_lock(&self.key, false, false)?;
        self.check_directory()?;
        Ok(locks)
    }

    pub fn incarnation(&self) -> &str {
        &self.incarnation
    }
    fn check_directory(&self) -> Result<()> {
        let current = self.stores.global().incarnation(&self.id, &self.key)?;
        ensure!(
            current.as_deref() == Some(&self.incarnation),
            "Session incarnation is no longer visible"
        );
        Ok(())
    }
    // Requires exclusive lifecycle access to the current incarnation.
    fn upgrade_locked(&self) -> Result<()> {
        let path = self.stores.sessions().join(&self.key).join("session.db");
        let _permit = Permits::take(1);
        if layout::needs_upgrade(&path, SESSION)? {
            let c = layout::open(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
            verify_manifest(
                &c,
                self.stores.global().store_uuid(),
                &self.id,
                &self.incarnation,
            )?;
            drop(c);
            layout::upgrade(&path, SESSION)?;
        }
        Ok(())
    }
    fn connection_under_lock<T>(
        &self,
        mode: OpenFlags,
        body: impl FnOnce(&mut Connection) -> Result<T>,
    ) -> Result<T> {
        self.connection_with_permit(Permits::take(1), mode, body)
    }
    fn connection_with_permit<T>(
        &self,
        _permit: Permits,
        mode: OpenFlags,
        body: impl FnOnce(&mut Connection) -> Result<T>,
    ) -> Result<T> {
        let path = self.stores.sessions().join(&self.key).join("session.db");
        let file = layout::file_identity(&path)?;
        let mut conn = layout::open(&path, mode)?;
        let snap = conn.transaction()?;
        let (schema, external_edit) = layout::session_header(&snap)?;
        verify_manifest(
            &snap,
            self.stores.global().store_uuid(),
            &self.id,
            &self.incarnation,
        )?;
        ensure!(
            file == layout::file_identity(&path)?,
            "Session file changed during checkout"
        );
        let identity = VerifiedShard {
            file,
            schema,
            incarnation: self.incarnation.clone(),
            external_edit,
        };
        let slot = self.stores.verification_slot(&self.key);
        {
            let mut verified = slot.lock().unwrap();
            if verified.as_ref() != Some(&identity) {
                *verified = None;
                layout::verify(&snap, SESSION)?;
                layout::integrity(&snap)?;
                *verified = Some(identity);
            }
        }
        snap.commit()?;
        body(&mut conn)
    }
    /// Use the supplied transaction for the duration of the callback. Nested
    /// store calls that need lifecycle access return an error on this thread.
    pub fn read<T>(&self, body: impl FnOnce(&SessionRead<'_>) -> Result<T>) -> Result<T> {
        let _locks = self.stores.key_lock(&self.key, false, false)?;
        self.binding_control()?;
        self.connection_under_lock(OpenFlags::SQLITE_OPEN_READ_ONLY, |c| {
            let tx = c.transaction()?;
            layout::callback(|| body(&SessionRead(&tx)))
        })
    }
    /// The closure follows the same re-entry rules as `read` and may run again
    /// after SQLite contention rolls back its transaction.
    pub fn write<T>(&self, body: impl FnMut(&SessionWrite<'_>) -> Result<T>) -> Result<T> {
        self.write_named("session write", body)
    }
    fn write_named<T>(
        &self,
        label: &str,
        body: impl FnMut(&SessionWrite<'_>) -> Result<T>,
    ) -> Result<T> {
        let _locks = self.stores.key_lock(&self.key, false, false)?;
        self.write_locked(label, body)
    }
    fn write_locked<T>(
        &self,
        label: &str,
        body: impl FnMut(&SessionWrite<'_>) -> Result<T>,
    ) -> Result<T> {
        self.write_locked_with_deadline(label,WRITE_DEADLINE,body)
    }
    fn write_locked_with_deadline<T>(
        &self,
        label: &str,
        deadline: Duration,
        body: impl FnMut(&SessionWrite<'_>) -> Result<T>,
    ) -> Result<T> {
        self.write_locked_with_commit(label, deadline, false, body, |tx| Ok(tx.commit()?))
    }

    fn write_locked_with_commit<T>(
        &self,
        label: &str,
        deadline: Duration,
        durable: bool,
        mut body: impl FnMut(&SessionWrite<'_>) -> Result<T>,
        mut commit:impl FnMut(rusqlite::Transaction<'_>) -> Result<()>,
    ) -> Result<T> {
        let start=Instant::now();
        let _admission=self.provider_fence(false,deadline)?;
        let binding = self.binding_control()?;
        let (result, projection) = self.connection_under_lock(OpenFlags::SQLITE_OPEN_READ_WRITE, |c| {
            if durable { c.pragma_update(None, "synchronous", "FULL")?; }
            crate::db::write_tx_with_commit(c, label, deadline.saturating_sub(start.elapsed()), |tx| {
                let before:i64=tx.query_row("SELECT total_changes()",[],|r|r.get(0))?;
                let writer=SessionWrite(&tx,&self.id,false.into(),&binding);
                let result=layout::callback(|| body(&writer))?;
                let changed:bool=tx.query_row("SELECT total_changes()>?1",[before],|r|r.get(0))?;
                let mut summary=None;
                if changed {
                    history::advance(&tx)?;
                    if writer.2.get() || !tx.query_row("SELECT EXISTS(SELECT 1 FROM execution_turns WHERE status='running')",[],|r|r.get::<_,bool>(0))? {
                        match history::projection(&tx) {
                            Ok(projection) => summary = Some(projection),
                            Err(error) => tracing::warn!(%error, session=%self.id, "Directory summary could not be computed"),
                        }
                    }
                }
                commit(tx)?;
                Ok((result,summary))
            })
        })
        .map_err(|e| crate::db::write_boundary_context(label, &self.id, start, e))?;
        drop(_admission);
        if let Some(p) = projection {
            if let Err(error) = self
                .stores
                .global()
                .project(&self.id, &self.incarnation, &p)
            {
                tracing::debug!(%error,session=%self.id,"Directory projection pending");
            }
        }
        Ok(result)
    }
    pub fn set_archived(&self, archived: bool) -> Result<()> {
        let _locks = self.stores.key_lock(&self.key, true, false)?;
        self.check_directory()?;
        self.stores
            .global()
            .write("archive session", &self.id, false, |tx| {
                tx.execute(
                    "UPDATE conversations SET archived=?1,metadata_revision=metadata_revision+1
                WHERE id=?2 AND incarnation=?3 AND archived<>?1",
                    rusqlite::params![archived, self.id, self.incarnation],
                )?;
                Ok(())
            })
    }
}

fn verify_manifest(c: &Connection, store: &str, id: &str, incarnation: &str) -> Result<()> {
    let matches: bool = c.query_row(
        "SELECT store_uuid=?1 AND id=?2 AND incarnation=?3 FROM manifest WHERE singleton=1",
        [store, id, incarnation],
        |r| r.get(0),
    )?;
    if !matches {
        return Err(layout::InvalidFormat("Session identity mismatch".into()).into());
    }
    Ok(())
}

pub struct SessionRead<'a>(&'a Connection);
impl SessionRead<'_> {
    pub fn initial_message(&self) -> Result<Option<FirstMessage>> {
        let pair = self
            .0
            .query_row(
                "SELECT m.id,m.conversation_id,m.role,m.content,m.thinking,m.created_at,
                        m.is_compact_boundary,m.sort_order,t.message_id,t.content_json
                 FROM messages m LEFT JOIN message_user_content t ON t.message_id=m.id
                 ORDER BY m.sort_order,m.rowid LIMIT 1",
                [],
                |row| {
                    let message = crate::db::message_repo::row_to_message(row)?;
                    let json = match row.get::<_, Option<String>>(8)? {
                        Some(_) => Some(row.get::<_, String>(9)?),
                        None => None,
                    };
                    Ok((message.id, message.content, json))
                },
            )
            .optional()?;
        pair.map(|(id, text, json)| {
            Ok(FirstMessage {
                origin: message_origin(self.0, &id)?.unwrap_or_else(|| "legacy".into()),
                text,
                typed: json.map(|value| serde_json::from_str(&value)).transpose()?,
            })
        })
        .transpose()
    }
}
pub struct SessionWrite<'a>(&'a Connection, &'a str, std::cell::Cell<bool>, &'a binding::BindingControl);

impl SessionWrite<'_> {
    #[doc(hidden)]
    pub fn synchronous_for_test(&self) -> Result<i32> {
        Ok(self
            .0
            .pragma_query_value(None, "synchronous", |r| r.get(0))?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pragmas(conn: &Connection) -> Result<(i32, String, i32, i32)> {
        Ok(conn.query_row(
            "SELECT synchronous,journal_mode,foreign_keys,timeout FROM
            pragma_synchronous,pragma_journal_mode,pragma_foreign_keys,pragma_busy_timeout",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )?)
    }

    #[test]
    fn turn_writes_share_the_wal_until_execution_finishes() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let stores = Stores::open(tmp.path())?;
        let session = stores.create("turn", Default::default())?;
        let lease = Arc::new(session.acquire_execution()?);
        let wal = stores.sessions().join(&session.key).join("session.db-wal");
        let guard = session.open_turn_wal(lease, Permits::take(1))?;
        session.write(|tx| tx.append("assistant", "saved"))?;
        assert!(std::fs::metadata(&wal)?.len() > 32);
        assert_eq!(
            session.read(|tx| tx.last_assistant_report())?.as_deref(),
            Some("saved")
        );
        drop(guard);
        assert!(!wal.exists());
        assert_eq!(
            session.read(|tx| tx.last_assistant_report())?.as_deref(),
            Some("saved")
        );
        session.delete()?;
        Ok(())
    }

    #[test]
    fn durability_and_busy_policy_follow_the_transaction_role() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let s = Stores::open(tmp.path())?;
        for publication in [true, false, true] {
            s.global()
                .write("inspect pragma", "test", publication, |tx| {
                    assert_eq!(
                        pragmas(tx)?,
                        (if publication { 2 } else { 1 }, "wal".into(), 1, 0)
                    );
                    Ok(())
                })?;
        }
        // I34 sets busy_timeout to zero after acquiring the writer. The next
        // checkout gets the normal acquisition budget back.
        s.global().read(|c| {
            assert_eq!(pragmas(c)?.3, 5000);
            Ok(())
        })?;
        let _permit = Permits::take(1);
        layout::initialize(&tmp.path().join("stage.db"), SESSION, |tx| {
            assert_eq!(pragmas(tx)?, (2, "wal".into(), 1, 5000));
            Ok(())
        })?;
        drop(_permit);
        s.create("test", CreateOptions::default())?.write(|tx| {
            assert_eq!(pragmas(tx.0)?, (1, "wal".into(), 1, 0));
            Ok(())
        })
    }

    #[cfg(unix)]
    #[test]
    fn multi_key_locks_are_unique_and_in_storage_key_order() -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("state/locks/session");
        std::fs::create_dir_all(&path)?;
        for name in ["a", "b"] {
            std::fs::File::create_new(path.join(name))?;
        }
        let locks = lock_keys(&path, vec!["b", "a", "b"], true)?;
        assert_eq!(locks.len(), 2);
        for (file, name) in locks.iter().zip(["a", "b"]) {
            assert_eq!(
                file.metadata()?.ino(),
                std::fs::metadata(path.join(name))?.ino()
            );
        }
        Ok(())
    }
}
