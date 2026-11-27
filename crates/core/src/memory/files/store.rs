use super::{
    format::{name_key, MemoryDocument, MAX_FILE_BYTES},
    hash, index, invalid, memory_filename,
    safe_dir::SafeDir,
    IndexContext, IndexLimits, MemoryFileError, MemoryScope, Result,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    fs::File,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

const MAX_INDEX_BYTES: usize = 8 * 1024 * 1024;
const MAX_JOURNAL_BYTES: usize = 128 * 1024 * 1024;
const MAX_SCAN_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone)]
pub enum ExpectedVersion {
    Absent,
    Hash(String),
}

impl ExpectedVersion {
    fn check(&self, filename: &str, bytes: Option<&[u8]>) -> Result<()> {
        let matches = match (self, bytes) {
            (Self::Absent, None) => true,
            (Self::Hash(expected), Some(bytes)) => expected == &hash(bytes),
            _ => false,
        };
        if matches {
            Ok(())
        } else {
            Err(MemoryFileError::Conflict(filename.into()))
        }
    }
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub filename: String,
    pub bytes: Vec<u8>,
    pub hash: String,
    pub document: MemoryDocument,
}

impl Entry {
    fn new(filename: String, bytes: Vec<u8>) -> Result<Self> {
        let document = MemoryDocument::parse(&bytes)?;
        Ok(Self {
            filename,
            hash: hash(&bytes),
            bytes,
            document,
        })
    }
}

#[derive(Debug, Clone)]
pub struct Diagnostic {
    pub filename: String,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub generation: u64,
    pub entries: Vec<Entry>,
    pub diagnostics: Vec<Diagnostic>,
    pub index: String,
    pub index_hash: String,
}

impl Snapshot {
    /// A confirmation binds the authored preimage and the complete proposed text.
    pub fn repair_preview(&self, scope: &str) -> Result<(String, String)> {
        let proposed = index::repair(&self.entries, &self.index);
        if proposed.len() > MAX_INDEX_BYTES {
            return Err(invalid("repaired index exceeds storage limit"));
        }
        let token =
            hash(format!("{scope}:{}:{}", self.index_hash, hash(proposed.as_bytes())).as_bytes());
        Ok((proposed, token))
    }

    pub fn index_context(&self, path: &str) -> IndexContext {
        IndexContext::from_index(&self.index, path, IndexLimits::default(), str::len)
    }
}

#[derive(Clone)]
pub struct StoreOptions {
    pub lock_timeout: Duration,
    pub cancelled: Arc<AtomicBool>,
}

impl Default for StoreOptions {
    fn default() -> Self {
        Self {
            lock_timeout: Duration::from_secs(2),
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// Synchronous, bounded local I/O. Async hosts should run a whole operation on a
/// blocking worker, not detach its commit. Cancellation is checked BEFORE the
/// durable journal commit point; after it, recover/finish and report actual state.
pub struct FileMemoryStore {
    directory: Arc<SafeDir>,
    state: Arc<SafeDir>,
    trash: Arc<SafeDir>,
    options: StoreOptions,
    read_only: bool,
}

struct ProjectLock {
    file: File,
    owner: u32,
}
impl Drop for ProjectLock {
    fn drop(&mut self) {
        if self.owner == std::process::id() {
            let _ = crate::fs::unlock_file(&self.file);
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Change {
    name: String,
    before: Option<Vec<u8>>,
    after: Option<Vec<u8>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u32,
    id: String,
    previous_generation: u64,
    generation: u64,
    previous_catalog: Option<Vec<u8>>,
    catalog: Vec<u8>,
    changes: Vec<Change>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalEnvelope {
    payload: Journal,
    checksum: String,
}

impl FileMemoryStore {
    pub fn open(data_dir: &Path, scope: &MemoryScope) -> Result<Self> {
        Self::open_with_options(data_dir, scope, StoreOptions::default())
    }

    pub fn open_with_options(
        data_dir: &Path,
        scope: &MemoryScope,
        options: StoreOptions,
    ) -> Result<Self> {
        Self::open_mode(data_dir, scope, options, false)
    }

    /// Never create files, repair an index or recover a journal in a read-only turn.
    pub fn open_read_only(data_dir: &Path, scope: &MemoryScope) -> Result<Self> {
        Self::open_mode(data_dir, scope, StoreOptions::default(), true)
    }

    /// Before the first store write there may already be hand-authored files,
    /// but no state directory or lock. Never bootstrap these just to list them.
    pub fn project_snapshot(data_dir: &Path, scope: &MemoryScope) -> Result<Snapshot> {
        let state = data_dir.join("data/memory/state").join(scope.key());
        let missing = |e: &MemoryFileError| matches!(e, MemoryFileError::Io(e) if e.kind() == std::io::ErrorKind::NotFound);
        match super::scoped_directory(data_dir, &state, true) {
            Err(e) if missing(&e) => (),
            other => {
                other?;
                return Self::open_read_only(data_dir, scope)?.snapshot();
            }
        }
        let directory = data_dir
            .join("data/memory/projects")
            .join(scope.key())
            .join("memory");
        let directory = match super::scoped_directory(data_dir, &directory, true) {
            Err(e) if missing(&e) => None,
            other => Some(other?),
        };
        let snapshot = Self::read_snapshot(directory.as_deref(), 0)?;
        // If a concurrent writer initialized the store, repeat under its lock.
        match super::scoped_directory(data_dir, &state, true) {
            Err(e) if missing(&e) => Ok(snapshot),
            other => {
                other?;
                Self::open_read_only(data_dir, scope)?.snapshot()
            }
        }
    }

    pub(crate) fn open_resident(root: &Path, directory: &Path, read_only: bool) -> Result<Self> {
        let directory = super::scoped_directory(root, directory, read_only)?;
        let state = directory.child_mode(".state", read_only)?;
        let trash = directory.child_mode(".trash", read_only)?;
        let store = Self {
            directory,
            state,
            trash,
            options: StoreOptions::default(),
            read_only,
        };
        let _lock = store.lock()?;
        store.recover()?;
        Ok(store)
    }

    pub(crate) fn read_resident_file(
        root: &Path,
        directory: &Path,
        filename: &str,
    ) -> Result<Option<Vec<u8>>> {
        if filename != "MEMORY.md" {
            memory_filename(filename)?;
        }
        super::scoped_directory(root, directory, true)?.read(filename, limit(filename))
    }

    pub(crate) fn write_resident_index(
        &self,
        bytes: &[u8],
        expected: ExpectedVersion,
    ) -> Result<String> {
        if self.read_only || bytes.len() > MAX_INDEX_BYTES || std::str::from_utf8(bytes).is_err() {
            return Err(invalid("invalid index write"));
        }
        let _lock = self.lock()?;
        self.recover()?;
        let old = self.directory.read("MEMORY.md", MAX_INDEX_BYTES)?;
        expected.check("MEMORY.md", old.as_deref())?;
        self.directory.atomic_write("MEMORY.md", bytes)?;
        Ok(hash(bytes))
    }

    fn open_mode(
        data_dir: &Path,
        scope: &MemoryScope,
        options: StoreOptions,
        read_only: bool,
    ) -> Result<Self> {
        if options.cancelled.load(Ordering::Acquire) {
            return Err(MemoryFileError::Cancelled);
        }
        let root = SafeDir::root(data_dir)?
            .child_mode("data", read_only)?
            .child_mode("memory", read_only)?;
        let state = root
            .child_mode("state", read_only)?
            .child_mode(scope.key(), read_only)?;
        let directory = root
            .child_mode("projects", read_only)?
            .child_mode(scope.key(), read_only)?
            .child_mode("memory", read_only)?;
        let trash = root
            .child_mode("trash", read_only)?
            .child_mode(scope.key(), read_only)?;
        let store = Self {
            directory,
            state,
            trash,
            options,
            read_only,
        };
        let _lock = store.lock()?;
        // Scope identity is persisted under the same project lock; hash collision
        // or mismatched binding fails closed instead of exposing another project.
        let identity = serde_json::to_vec(scope).map_err(|_| invalid("scope serialization"))?;
        match store.state.read("scope.json", 1024 * 1024)? {
            Some(saved) if saved != identity => {
                return Err(invalid("scope identity mismatch; explicit rebind required"))
            }
            None if read_only => return Err(invalid("memory scope is not initialized")),
            None => store.state.atomic_write("scope.json", &identity)?,
            _ => {}
        }
        store.recover()?;
        Ok(store)
    }

    pub fn directory(&self) -> &Path {
        self.directory.path()
    }
    pub fn index_path(&self) -> PathBuf {
        self.directory.path().join("MEMORY.md")
    }

    fn cancelled(&self) -> Result<()> {
        if self.options.cancelled.load(Ordering::Acquire) {
            Err(MemoryFileError::Cancelled)
        } else {
            Ok(())
        }
    }

    fn lock(&self) -> Result<ProjectLock> {
        self.lock_mode(self.read_only)
    }

    fn lock_mode(&self, read_only: bool) -> Result<ProjectLock> {
        self.cancelled()?;
        let file = self.state.lock_file(read_only)?;
        let start = Instant::now();
        loop {
            self.cancelled()?;
            match crate::fs::try_lock_file(&file) {
                Ok(()) => {
                    let lock = ProjectLock {
                        file,
                        owner: std::process::id(),
                    };
                    self.state.verify_file("lock", &lock.file)?;
                    self.directory.verify()?;
                    self.trash.verify()?;
                    return Ok(lock);
                }
                Err(std::fs::TryLockError::WouldBlock)
                    if start.elapsed() < self.options.lock_timeout =>
                {
                    std::thread::sleep(Duration::from_millis(5))
                }
                Err(std::fs::TryLockError::WouldBlock) => return Err(MemoryFileError::Busy),
                Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
            }
        }
    }

    fn generation(&self) -> Result<u64> {
        match self.state.read("generation", 32)? {
            None => Ok(0),
            Some(bytes) => std::str::from_utf8(&bytes)
                .ok()
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| invalid("corrupt generation")),
        }
    }

    pub fn read(&self, filename: &str) -> Result<Option<Entry>> {
        memory_filename(filename)?;
        let _lock = self.lock_mode(true)?;
        self.ensure_clean()?;
        self.directory
            .read(filename, MAX_FILE_BYTES)?
            .map(|bytes| Entry::new(filename.into(), bytes))
            .transpose()
    }

    /// Hash CAS also permits deleting an invalid external file deliberately: use
    /// read_bytes to inspect it first. No malformed file is silently deleted.
    pub fn read_bytes(&self, filename: &str) -> Result<Option<Vec<u8>>> {
        memory_filename(filename)?;
        let _lock = self.lock_mode(true)?;
        self.ensure_clean()?;
        self.directory.read(filename, MAX_FILE_BYTES)
    }

    pub fn put(&self, filename: &str, bytes: &[u8], expected: ExpectedVersion) -> Result<Entry> {
        if self.read_only {
            return Err(invalid("memory is read-only"));
        }
        memory_filename(filename)?;
        let document = MemoryDocument::parse(bytes)?;
        let _lock = self.lock()?;
        self.recover()?;
        let before = self.directory.read(filename, MAX_FILE_BYTES)?;
        expected.check(filename, before.as_deref())?;
        let previous = before.as_deref().map(MemoryDocument::parse).transpose()?;
        let after = document.prepare(previous.as_ref())?;
        let entry = Entry::new(filename.into(), after.clone())?;
        self.commit_topic(Change {
            name: filename.into(),
            before,
            after: Some(after),
        })?;
        Ok(entry)
    }

    pub fn delete(&self, filename: &str, expected: ExpectedVersion) -> Result<()> {
        if self.read_only {
            return Err(invalid("memory is read-only"));
        }
        memory_filename(filename)?;
        let _lock = self.lock()?;
        self.recover()?;
        let before = self.directory.read(filename, MAX_FILE_BYTES)?;
        expected.check(filename, before.as_deref())?;
        if before.is_none() {
            return Err(invalid("memory not found"));
        }
        self.commit_topic(Change {
            name: filename.into(),
            before,
            after: None,
        })?;
        Ok(())
    }

    pub fn snapshot(&self) -> Result<Snapshot> {
        let _lock = self.lock_mode(true)?;
        self.ensure_clean()?;
        Self::read_snapshot(Some(&self.directory), self.generation()?)
    }

    fn read_snapshot(directory: Option<&SafeDir>, generation: u64) -> Result<Snapshot> {
        let (entries, diagnostics, bytes) = match directory {
            Some(dir) => {
                let (entries, diagnostics) = Self::scan(dir, None)?;
                (
                    entries,
                    diagnostics,
                    dir.read("MEMORY.md", MAX_INDEX_BYTES)?.unwrap_or_default(),
                )
            }
            None => (Vec::new(), Vec::new(), Vec::new()),
        };
        let index = String::from_utf8(bytes).map_err(|_| invalid("index is not UTF-8"))?;
        Ok(Snapshot {
            generation,
            index_hash: hash(index.as_bytes()),
            index,
            entries,
            diagnostics,
        })
    }

    pub fn confirm_repair(&self, scope: &str, confirmation: &str) -> Result<()> {
        if self.read_only {
            return Err(invalid("memory is read-only"));
        }
        let _lock = self.lock()?;
        self.recover()?;
        let snapshot = Self::read_snapshot(Some(&self.directory), self.generation()?)?;
        let (proposed, token) = snapshot.repair_preview(scope)?;
        if token != confirmation {
            return Err(MemoryFileError::Conflict("catalog repair preview".into()));
        }
        if proposed != snapshot.index {
            let before = self.directory.read("MEMORY.md", MAX_INDEX_BYTES)?;
            if before.as_deref().unwrap_or_default() != snapshot.index.as_bytes() {
                return Err(MemoryFileError::Conflict("MEMORY.md".into()));
            }
            self.commit(
                vec![Change {
                    name: "MEMORY.md".into(),
                    before,
                    after: Some(proposed.into_bytes()),
                }],
                Self::catalog(&snapshot.entries)?,
            )?;
        }
        Ok(())
    }

    fn scan(directory: &SafeDir, change: Option<&Change>) -> Result<(Vec<Entry>, Vec<Diagnostic>)> {
        let mut files: BTreeMap<String, Option<Vec<u8>>> = directory
            .list()?
            .into_iter()
            .filter(|name| name.ends_with(".md") && name != "MEMORY.md")
            .map(|name| (name, None))
            .collect();
        if let Some(change) = change {
            if let Some(bytes) = &change.after {
                files.insert(change.name.clone(), Some(bytes.clone()));
            } else {
                files.remove(&change.name);
            }
        }
        if files.len() > 10_000 {
            return Err(invalid("memory file count exceeds 10000"));
        }
        let (mut entries, mut diagnostics) = (Vec::new(), Vec::new());
        let mut total = 0usize;
        for (name, supplied) in files {
            let parsed = (|| {
                memory_filename(&name)?;
                let bytes = match supplied {
                    Some(bytes) => bytes,
                    None => directory
                        .read(&name, MAX_FILE_BYTES)?
                        .ok_or_else(|| MemoryFileError::Conflict(name.clone()))?,
                };
                total = total.saturating_add(bytes.len());
                Entry::new(name.clone(), bytes)
            })();
            if total > MAX_SCAN_BYTES {
                return Err(invalid("active memory bytes exceed 64 MiB"));
            }
            match parsed {
                Ok(entry) => entries.push(entry),
                Err(e) => diagnostics.push(Diagnostic {
                    filename: name,
                    message: e.to_string(),
                }),
            }
        }
        let mut names = HashMap::<String, usize>::new();
        let mut ids = HashMap::<String, usize>::new();
        for entry in &entries {
            *names.entry(name_key(&entry.document.name)).or_default() += 1;
            if let Some(id) = entry.document.id() {
                *ids.entry(id.into()).or_default() += 1;
            }
        }
        let duplicate = |entry: &Entry| {
            names[&name_key(&entry.document.name)] > 1
                || entry.document.id().is_some_and(|id| ids[id] > 1)
        };
        if change.is_some_and(|c| c.after.is_some()) && entries.iter().any(duplicate) {
            return Err(invalid(
                "duplicate memory name or id; resolve conflict before writing",
            ));
        }
        entries.retain(|entry| {
            if duplicate(entry) {
                diagnostics.push(Diagnostic {
                    filename: entry.filename.clone(),
                    message: "duplicate name or id; omitted from index".into(),
                });
                false
            } else {
                true
            }
        });
        for entry in &entries {
            for link in entry.document.links() {
                if names.get(&name_key(&link)).copied() != Some(1) {
                    diagnostics.push(Diagnostic {
                        filename: entry.filename.clone(),
                        message: "unresolved or ambiguous memory link".into(),
                    });
                }
            }
        }
        Ok((entries, diagnostics))
    }

    fn commit_topic(&self, change: Change) -> Result<()> {
        let (entries, _) = Self::scan(&self.directory, Some(&change))?;
        self.commit(vec![change], Self::catalog(&entries)?)
    }

    fn catalog(entries: &[Entry]) -> Result<Vec<u8>> {
        Ok(hash(
            &serde_json::to_vec(
                &entries
                    .iter()
                    .map(|entry| (&entry.filename, &entry.hash))
                    .collect::<Vec<_>>(),
            )
            .map_err(|_| invalid("catalog serialization"))?,
        )
        .into_bytes())
    }

    fn commit(&self, changes: Vec<Change>, catalog: Vec<u8>) -> Result<()> {
        let previous_generation = self.generation()?;
        let journal = Journal {
            version: 2,
            id: uuid::Uuid::new_v4().to_string(),
            previous_generation,
            generation: previous_generation
                .checked_add(1)
                .ok_or_else(|| invalid("generation overflow"))?,
            changes,
            previous_catalog: self.state.read("catalog", 64)?,
            catalog,
        };
        self.preflight(&journal)?;
        // Stage on the same directory/volume as the destination. Journal contains
        // complete payloads too, allowing recovery after a lost staging entry.
        for (i, change) in journal.changes.iter().enumerate() {
            if let Some(bytes) = &change.after {
                self.directory.create(&stage(&journal, i), bytes)?;
            }
        }
        checkpoint("staged");
        self.cancelled()?;
        self.preflight(&journal)?;
        let payload =
            serde_json::to_vec(&journal).map_err(|_| invalid("journal serialization failed"))?;
        let envelope = JournalEnvelope {
            checksum: hash(&payload),
            payload: journal,
        };
        let bytes =
            serde_json::to_vec(&envelope).map_err(|_| invalid("journal serialization failed"))?;
        if bytes.len() > MAX_JOURNAL_BYTES {
            return Err(invalid("journal too large"));
        }
        self.state.atomic_write("journal.json", &bytes)?;
        checkpoint("journal");
        // This is the commit point. No cancellation/unowned background write.
        self.finish(&envelope.payload)
    }

    fn ensure_clean(&self) -> Result<()> {
        if self
            .state
            .read("journal.json", MAX_JOURNAL_BYTES)?
            .is_some()
        {
            return Err(invalid(
                "memory recovery requires a writable maintenance operation",
            ));
        }
        Ok(())
    }

    fn recover(&self) -> Result<()> {
        let Some(bytes) = self.state.read("journal.json", MAX_JOURNAL_BYTES)? else {
            return Ok(());
        };
        if self.read_only {
            return Err(invalid(
                "memory recovery requires a writable maintenance operation",
            ));
        }
        let envelope: JournalEnvelope = serde_json::from_slice(&bytes)
            .map_err(|_| invalid("corrupt memory journal; preserved for recovery"))?;
        let payload = serde_json::to_vec(&envelope.payload)
            .map_err(|_| invalid("journal serialization failed"))?;
        if hash(&payload) != envelope.checksum {
            return Err(invalid("journal checksum mismatch; preserved for recovery"));
        }
        self.finish(&envelope.payload)
    }

    fn preflight(&self, journal: &Journal) -> Result<()> {
        if !matches!(journal.version, 1 | 2)
            || uuid::Uuid::parse_str(&journal.id).is_err()
            || journal.changes.len() > 2
            || journal.previous_generation.checked_add(1) != Some(journal.generation)
        {
            return Err(invalid("invalid journal header"));
        }
        let valid_hash =
            |bytes: &[u8]| bytes.len() == 64 && bytes.iter().all(u8::is_ascii_hexdigit);
        if !valid_hash(&journal.catalog)
            || journal
                .previous_catalog
                .as_ref()
                .is_some_and(|bytes| !valid_hash(bytes))
        {
            return Err(invalid("invalid journal catalog hash"));
        }
        let catalog = self.state.read("catalog", 64)?;
        if catalog != journal.previous_catalog
            && catalog.as_deref() != Some(journal.catalog.as_slice())
        {
            return Err(MemoryFileError::RecoveryConflict("catalog".into()));
        }
        let generation = self.generation()?;
        if generation != journal.previous_generation && generation != journal.generation {
            return Err(MemoryFileError::RecoveryConflict("generation".into()));
        }
        let mut seen = std::collections::HashSet::new();
        for change in &journal.changes {
            if change.name != "MEMORY.md" {
                memory_filename(&change.name)?;
            }
            if !seen.insert(&change.name) {
                return Err(invalid("duplicate journal path"));
            }
            let limit = limit(&change.name);
            if change.before.as_ref().is_some_and(|v| v.len() > limit)
                || change.after.as_ref().is_some_and(|v| v.len() > limit)
            {
                return Err(invalid("journal payload exceeds limit"));
            }
            if let Some(after) = &change.after {
                if change.name == "MEMORY.md" {
                    std::str::from_utf8(after).map_err(|_| invalid("invalid journal index"))?;
                } else {
                    MemoryDocument::parse(after)?;
                }
            }
            let current = self.directory.read(&change.name, limit)?;
            // A pre-upgrade journal could contain an implicit catalog rewrite.
            // Never roll it forward over an authored preimage on upgrade.
            if journal.version == 1 && change.name == "MEMORY.md" && current != change.after {
                return Err(MemoryFileError::RecoveryConflict(
                    "unapplied legacy index rewrite".into(),
                ));
            }
            if current != change.before && current != change.after {
                return Err(MemoryFileError::RecoveryConflict(change.name.clone()));
            }
        }
        Ok(())
    }

    fn finish(&self, journal: &Journal) -> Result<()> {
        self.preflight(journal)?;
        let backup = self.trash.child(&journal.id)?;
        for (i, change) in journal.changes.iter().enumerate() {
            if let Some(before) = &change.before {
                match backup.read(&change.name, limit(&change.name))? {
                    Some(saved) if saved != *before => {
                        return Err(MemoryFileError::RecoveryConflict("backup changed".into()))
                    }
                    None => backup.create(&change.name, before)?,
                    _ => {}
                }
            }
            let current = self.directory.read(&change.name, limit(&change.name))?;
            if current == change.after {
                continue;
            }
            if current != change.before {
                return Err(MemoryFileError::RecoveryConflict(change.name.clone()));
            }
            if let Some(bytes) = &change.after {
                let stage = stage(journal, i);
                match self.directory.read(&stage, limit(&change.name))? {
                    None => self.directory.create(&stage, bytes)?,
                    Some(saved) if saved != *bytes => {
                        return Err(MemoryFileError::RecoveryConflict(
                            "staged payload changed".into(),
                        ))
                    }
                    _ => {}
                }
                self.directory.replace(&stage, &change.name)?;
            } else {
                self.directory.remove(&change.name)?;
            }
            checkpoint(if change.name == "MEMORY.md" {
                "index"
            } else {
                "entry"
            });
        }
        self.directory.sync()?;
        self.state.atomic_write("catalog", &journal.catalog)?;
        checkpoint("catalog");
        self.state
            .atomic_write("generation", journal.generation.to_string().as_bytes())?;
        checkpoint("generation");
        self.state.remove("journal.json")?;
        checkpoint("cleared");
        for i in 0..journal.changes.len() {
            self.directory.remove(&stage(journal, i))?;
        }
        Ok(())
    }
}

fn stage(journal: &Journal, i: usize) -> String {
    format!("stage-{}-{i}", journal.id)
}
fn limit(name: &str) -> usize {
    if name == "MEMORY.md" {
        MAX_INDEX_BYTES
    } else {
        MAX_FILE_BYTES
    }
}

#[cfg(not(test))]
fn checkpoint(_phase: &str) {}

#[cfg(test)]
fn checkpoint(phase: &str) {
    // Only a privately launched test process sets this variable. No production
    // failpoint, environment switch, or public mutation hook is compiled in.
    if std::env::var("DCL_MEMORY_TEST_CRASH").ok().as_deref() == Some(phase) {
        std::process::exit(87);
    }
}
