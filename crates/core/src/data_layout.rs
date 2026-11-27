//! The on-disk contract for the shared data directory.
//!
//! Everything that lives below the data directory has exactly one home here.
//! Callers must not join top-level names onto the data directory themselves:
//! a new entry is either a root anchor listed in [`ROOT_ALLOWLIST`] or a child
//! of one of the seven lifecycle category directories.
//!
//! A v1 directory is migrated once, in place, by [`DataLayout::migrate_if_needed`].
//! The migration is a sequence of same-volume renames recorded in `layout.json`,
//! so a crash resumes instead of duplicating or losing data.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Layout contract version written to `layout.json`.
pub const LAYOUT_VERSION: u32 = 2;
/// Version of a directory that predates `layout.json`.
const LEGACY_VERSION: u32 = 1;
pub const LAYOUT_FILE: &str = "layout.json";
pub const LAYOUT_LOCK: &str = ".layout.lock";
pub const README_FILE: &str = "README.md";

pub const CONFIG_DIR: &str = "config";
pub const DATA_DIR: &str = "data";
pub const STATE_DIR: &str = "state";
pub const TASKS_DIR: &str = "tasks";
pub const CACHE_DIR: &str = "cache";
pub const DIAGNOSTICS_DIR: &str = "diagnostics";
pub const RESOURCES_DIR: &str = "resources";

/// The seven lifecycle categories. Creating one is an explicit act of the code
/// that writes into it; a fresh root stays empty until something is used.
pub const CATEGORY_DIRS: &[&str] = &[
    CONFIG_DIR,
    DATA_DIR,
    STATE_DIR,
    TASKS_DIR,
    CACHE_DIR,
    DIAGNOSTICS_DIR,
    RESOURCES_DIR,
];

/// Every entry allowed directly in the data directory root.
pub const ROOT_ALLOWLIST: &[&str] = &[
    // Root anchors.
    "global.db",
    "global.db-wal",
    "global.db-shm",
    "global.db-journal",
    LAYOUT_FILE,
    README_FILE,
    "AGENTS.md",
    "CLAUDE.md",
    "skills",
    "mcp-servers.json",
    "mcp-servers.lock",
    // Transient while a migration is running.
    LAYOUT_LOCK,
    // Category directories.
    CONFIG_DIR,
    DATA_DIR,
    STATE_DIR,
    TASKS_DIR,
    CACHE_DIR,
    DIAGNOSTICS_DIR,
    RESOURCES_DIR,
];

/// OS metadata files that are not part of the contract and are never reported.
const IGNORED_ROOT_ENTRIES: &[&str] = &[".DS_Store", "Thumbs.db", "desktop.ini", ".localized"];

const MIGRATION_LOCK_WAIT: Duration = Duration::from_secs(30);
const LIVE_LOCK_POLL: Duration = Duration::from_millis(50);

/// Result of evaluating the directory on startup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutStatus {
    /// A v2 directory that needed no migration.
    Ready,
    /// A v1 directory that was migrated by this call.
    Migrated,
    /// An empty directory that received `layout.json` and `README.md`.
    Fresh,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutState {
    pub status: LayoutStatus,
    /// Legacy or unknown root entries that were left in place.
    pub warnings: Vec<String>,
}

/// Read-only view of `layout.json` and the root entries it governs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutDiagnostics {
    /// `None` when the data directory has no `layout.json` at all.
    pub version: Option<u32>,
    /// `"migrating"` or `"ready"`.
    pub state: Option<&'static str>,
    /// Known v1 directories and files that still sit in the root.
    pub legacy_entries: Vec<String>,
    /// Root entries that are neither allowlisted nor known legacy names.
    pub unexpected_entries: Vec<String>,
}

/// The single source of every top-level path below the data directory.
#[derive(Clone, Debug)]
pub struct DataLayout {
    root: PathBuf,
}

impl DataLayout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn from_root(root: impl AsRef<Path>) -> Self {
        Self::new(root.as_ref().to_path_buf())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    // ---- category roots -------------------------------------------------

    pub fn config_root(&self) -> PathBuf {
        self.root.join(CONFIG_DIR)
    }

    pub fn data_root(&self) -> PathBuf {
        self.root.join(DATA_DIR)
    }

    pub fn state_root(&self) -> PathBuf {
        self.root.join(STATE_DIR)
    }

    pub fn tasks_root(&self) -> PathBuf {
        self.root.join(TASKS_DIR)
    }

    pub fn cache_root(&self) -> PathBuf {
        self.root.join(CACHE_DIR)
    }

    pub fn diagnostics_root(&self) -> PathBuf {
        self.root.join(DIAGNOSTICS_DIR)
    }

    pub fn resources_root(&self) -> PathBuf {
        self.root.join(RESOURCES_DIR)
    }

    // ---- root anchors ---------------------------------------------------

    pub fn global_db(&self) -> PathBuf {
        self.root.join("global.db")
    }

    pub fn sessions(&self) -> PathBuf {
        self.data_path("sessions")
    }

    pub fn global_blobs(&self) -> PathBuf {
        self.data_path("global-blobs")
    }

    pub fn session_tasks(&self) -> PathBuf {
        self.data_path("session-tasks")
    }

    pub fn workspaces(&self) -> PathBuf {
        self.data_path("workspaces")
    }

    pub fn pasted_images(&self) -> PathBuf {
        self.data_path("pasted-images")
    }

    pub fn screenshots(&self) -> PathBuf {
        self.cache_dir("screenshots")
    }

    pub fn memory_db(&self) -> PathBuf {
        self.data_path("memory.sqlite3")
    }

    pub fn team_server_db(&self) -> PathBuf {
        self.data_path("team-server.sqlite3")
    }

    pub fn agent_messages_db(&self) -> PathBuf {
        self.data_path("agent-messages.sqlite3")
    }

    pub fn agent_memory(&self) -> PathBuf {
        self.data_path("agent-memory")
    }

    pub fn memory_files(&self) -> PathBuf {
        self.data_path("memory")
    }

    pub fn teams(&self) -> PathBuf {
        self.data_path("teams")
    }

    pub fn schedules_db(&self) -> PathBuf {
        self.data_path("schedules.sqlite3")
    }

    pub fn costs(&self) -> PathBuf {
        self.data_path("costs")
    }

    pub fn knowledge(&self) -> PathBuf {
        self.data_path("knowledge")
    }

    pub fn analytics(&self) -> PathBuf {
        self.data_path("analytics")
    }

    pub fn uploads(&self) -> PathBuf {
        self.data_path("uploads")
    }

    pub fn app_studio(&self) -> PathBuf {
        self.data_path("app-studio")
    }

    pub fn composio_sessions(&self) -> PathBuf {
        self.data_path("composio_sessions.json")
    }

    pub fn backups(&self) -> PathBuf {
        self.data_path("backups")
    }

    pub fn legacy_data(&self) -> PathBuf {
        self.data_path("legacy")
    }

    pub fn skills(&self) -> PathBuf {
        self.root.join("skills")
    }

    pub fn lsp(&self) -> PathBuf {
        self.resources_dir("lsp")
    }

    pub fn linux_rootfs(&self) -> PathBuf {
        self.resources_dir("linux").join("rootfs")
    }

    /// A root file. Only names in [`ROOT_ALLOWLIST`] are accepted.
    pub fn root_file(&self, name: &str) -> PathBuf {
        debug_assert!(
            ROOT_ALLOWLIST.contains(&name),
            "unregistered root entry: {name}"
        );
        self.root.join(name)
    }

    pub fn config_file(&self, name: &str) -> PathBuf {
        self.config_root().join(name)
    }

    pub fn mcp_states(&self) -> PathBuf {
        self.config_file("mcp-states.json")
    }

    pub fn agent_models(&self) -> PathBuf {
        self.config_file("agent-models.json")
    }

    pub fn remote_control_settings(&self) -> PathBuf {
        self.config_file("remote-control.json")
    }

    pub fn github_token(&self) -> PathBuf {
        self.config_file("github-token.json")
    }

    /// `config/backups/` for the timestamped copies of `config.json`.
    pub fn config_backups(&self) -> PathBuf {
        self.config_root().join("backups")
    }

    // ---- category children ----------------------------------------------

    pub fn data_path(&self, name: &str) -> PathBuf {
        self.data_root().join(name)
    }

    pub fn config_dir(&self, name: &str) -> PathBuf {
        self.config_root().join(name)
    }

    pub fn state_dir(&self, name: &str) -> PathBuf {
        self.state_root().join(name)
    }

    /// `state/locks/<name>`; pass `session`, `workspace`, ... for a lock dir.
    pub fn lock_path(&self, name: &str) -> PathBuf {
        self.state_dir("locks").join(name)
    }

    pub fn task_dir(&self, name: &str) -> PathBuf {
        self.tasks_root().join(name)
    }

    pub fn cache_dir(&self, name: &str) -> PathBuf {
        self.cache_root().join(name)
    }

    pub fn diagnostics_dir(&self, name: &str) -> PathBuf {
        self.diagnostics_root().join(name)
    }

    pub fn resources_dir(&self, name: &str) -> PathBuf {
        self.resources_root().join(name)
    }

    // ---- named state ----------------------------------------------------

    pub fn bootstrap(&self) -> PathBuf {
        self.state_dir("bootstrap")
    }

    pub fn staging(&self) -> PathBuf {
        self.state_dir("staging")
    }

    pub fn presence(&self) -> PathBuf {
        self.state_dir("presence")
    }

    pub fn workers(&self) -> PathBuf {
        self.state_dir("workers")
    }

    pub fn turn_completions(&self) -> PathBuf {
        self.state_dir("turn-completions")
    }

    pub fn shell_cwd(&self) -> PathBuf {
        self.state_dir("shell-cwd")
    }

    pub fn session_env(&self) -> PathBuf {
        self.state_dir("session-env")
    }

    pub fn publication_lock(&self) -> PathBuf {
        self.lock_path("publication.lock")
    }

    pub fn server_token(&self) -> PathBuf {
        self.state_dir("server-token")
    }

    pub fn dev_bridge_key(&self) -> PathBuf {
        self.state_dir("dev-bridge-key")
    }

    pub fn hook_once(&self) -> PathBuf {
        self.state_dir("hook-once")
    }

    pub fn plugin_runs(&self) -> PathBuf {
        self.state_dir("plugin-runs")
    }

    pub fn skill_runs(&self) -> PathBuf {
        self.state_dir("skill-runs")
    }

    pub fn orchestrator_config(&self) -> PathBuf {
        self.config_file("orchestrator.toml")
    }

    pub fn migrated_marker(&self) -> PathBuf {
        self.state_dir("migrated")
    }

    // ---- named tasks ----------------------------------------------------

    pub fn background_tasks(&self) -> PathBuf {
        self.task_dir("background")
    }

    pub fn shell_tasks(&self) -> PathBuf {
        self.task_dir("shell")
    }

    pub fn detached_shells(&self) -> PathBuf {
        self.task_dir("detached-shells")
    }

    pub fn checkpoints(&self) -> PathBuf {
        self.task_dir("checkpoints")
    }

    pub fn plans(&self) -> PathBuf {
        self.task_dir("plans")
    }

    pub fn turn_context(&self) -> PathBuf {
        self.task_dir("turn-context")
    }

    pub fn design_plans(&self) -> PathBuf {
        self.task_dir("design-plans")
    }

    pub fn super_agent(&self) -> PathBuf {
        self.task_dir("super-agent")
    }

    pub fn toolscript_jobs(&self) -> PathBuf {
        self.task_dir("toolscript-jobs")
    }

    pub fn workflows(&self) -> PathBuf {
        self.task_dir("workflows")
    }

    pub fn workplans(&self) -> PathBuf {
        self.task_dir("workplans")
    }

    pub fn wakeups(&self) -> PathBuf {
        self.task_dir("wakeups")
    }

    // ---- named cache ----------------------------------------------------

    pub fn environment_files(&self) -> PathBuf {
        self.cache_dir("environment")
    }

    pub fn mcp_tool_snapshots(&self) -> PathBuf {
        self.cache_dir("mcp-tool-snapshots")
    }

    pub fn tool_outputs(&self) -> PathBuf {
        self.cache_dir("tool-outputs")
    }

    pub fn tool_output_metadata(&self) -> PathBuf {
        self.cache_dir("tool-output-metadata")
    }

    pub fn git_status(&self) -> PathBuf {
        self.cache_dir("git-status")
    }

    pub fn file_state(&self) -> PathBuf {
        self.cache_dir("file-state")
    }

    pub fn compactions(&self) -> PathBuf {
        self.cache_dir("compactions")
    }

    pub fn updates(&self) -> PathBuf {
        self.cache_dir("updates")
    }

    pub fn plugins_cache(&self) -> PathBuf {
        self.cache_dir("plugins")
    }

    // ---- named diagnostics ----------------------------------------------

    pub fn request_records(&self) -> PathBuf {
        self.diagnostics_dir("request-records")
    }

    pub fn logs(&self) -> PathBuf {
        self.diagnostics_dir("logs")
    }

    pub fn debug_logs(&self) -> PathBuf {
        self.diagnostics_dir("debug")
    }

    pub fn design_conversions(&self) -> PathBuf {
        self.diagnostics_dir("design-conversions")
    }

    pub fn hook_transcripts(&self) -> PathBuf {
        self.diagnostics_dir("hook-transcripts")
    }

    // ---- migration ------------------------------------------------------

    pub fn layout_file(&self) -> PathBuf {
        self.root.join(LAYOUT_FILE)
    }

    pub fn migrate_if_needed(&self) -> Result<LayoutState> {
        // Read-only commands must not create a data directory that the user
        // never asked for. Stores::open creates it before calling us.
        if !self.root.try_exists()? {
            return Ok(LayoutState {
                status: LayoutStatus::Fresh,
                warnings: Vec::new(),
            });
        }
        let existing = read_layout(&self.root)?;
        if let Some(layout) = &existing {
            if layout.version > LAYOUT_VERSION {
                bail!(
                    "Data directory {} was created by a newer version (layout {}); upgrade dcl/Desktop",
                    self.root.display(),
                    layout.version
                );
            }
            if layout.version == LAYOUT_VERSION && layout.state == LayoutStateName::Ready {
                write_readme(&self.root)?;
                return Ok(LayoutState {
                    status: LayoutStatus::Ready,
                    warnings: self.legacy_warnings()?,
                });
            }
        }

        let lock = self.acquire_migration_lock()?;
        // The previous owner may have finished while this process waited.
        let existing = read_layout(&self.root)?;
        if let Some(layout) = &existing {
            if layout.version > LAYOUT_VERSION {
                drop(lock);
                bail!(
                    "Data directory {} was created by a newer version (layout {}); upgrade dcl/Desktop",
                    self.root.display(),
                    layout.version
                );
            }
            if layout.version == LAYOUT_VERSION && layout.state == LayoutStateName::Ready {
                drop(lock);
                write_readme(&self.root)?;
                return Ok(LayoutState {
                    status: LayoutStatus::Ready,
                    warnings: self.legacy_warnings()?,
                });
            }
        }

        let result = self.run_migration(existing, &lock);
        drop(lock);
        result
    }

    fn acquire_migration_lock(&self) -> Result<MigrationLock> {
        let path = self.root.join(LAYOUT_LOCK);
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("Opening migration lock {}", path.display()))?;
        let deadline = Instant::now() + MIGRATION_LOCK_WAIT;
        loop {
            match file.try_lock() {
                Ok(()) => {
                    return Ok(MigrationLock {
                        file: Some(file),
                        path,
                    })
                }
                Err(fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(LIVE_LOCK_POLL);
                }
                Err(fs::TryLockError::WouldBlock) => {
                    bail!(
                        "Another process is still migrating {}; wait for it to finish and retry",
                        self.root.display()
                    )
                }
                Err(fs::TryLockError::Error(error)) => {
                    return Err(error).with_context(|| format!("Locking {}", path.display()))
                }
            }
        }
    }

    fn run_migration(
        &self,
        existing: Option<LayoutFile>,
        _lock: &MigrationLock,
    ) -> Result<LayoutState> {
        let mut moves = match &existing {
            Some(layout) if layout.state == LayoutStateName::Migrating => {
                let mut moves = layout.moves.clone();
                self.extend_planned(&mut moves)?;
                moves
            }
            _ => self.plan_moves()?,
        };
        let from_version = existing
            .as_ref()
            .map_or(LEGACY_VERSION, |layout| layout.version);
        let resumed = existing
            .as_ref()
            .is_some_and(|layout| layout.state == LayoutStateName::Migrating);
        let fresh = existing.is_none() && moves.is_empty();
        let started_at = existing
            .as_ref()
            .and_then(|layout| layout.started_at.clone())
            .unwrap_or_else(now);

        if !moves.is_empty() {
            if let Some(active) = self.active_instance()? {
                bail!(
                    "Another DeepSeek Code / dcl / Desktop instance is running ({}); exit all instances and retry",
                    active.display()
                );
            }
        }

        let mut layout = LayoutFile {
            version: LAYOUT_VERSION,
            state: LayoutStateName::Migrating,
            started_at: Some(started_at),
            migrated_at: None,
            from_version: Some(from_version),
            moves: moves.clone(),
            warnings: Vec::new(),
        };
        write_layout(&self.root, &layout)?;

        for index in 0..moves.len() {
            if moves[index].status == MoveStatus::Done {
                continue;
            }
            let source = self.root.join(&moves[index].from);
            let target = self.root.join(&moves[index].to);
            if source.try_exists()? && target.try_exists()? {
                bail!(
                    "Layout conflict: both {} and {} exist; move or remove one manually, then retry",
                    source.display(),
                    target.display()
                );
            }
            if source.try_exists()? {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent).with_context(|| {
                        format!("Creating layout directory {}", parent.display())
                    })?;
                }
                rename_with_retry(&source, &target).with_context(|| {
                    format!(
                        "Moving {} to {}; close any process using it and retry",
                        source.display(),
                        target.display()
                    )
                })?;
            }
            moves[index].status = MoveStatus::Done;
            layout.moves = moves.clone();
            write_layout(&self.root, &layout)?;
        }

        let warnings = self.final_warnings()?;
        layout.state = LayoutStateName::Ready;
        layout.migrated_at = Some(now());
        layout.started_at = None;
        layout.from_version = None;
        layout.moves.clear();
        layout.warnings.clear();
        write_layout(&self.root, &layout)?;
        write_readme(&self.root)?;
        Ok(LayoutState {
            status: if fresh {
                LayoutStatus::Fresh
            } else if resumed || from_version < LAYOUT_VERSION {
                LayoutStatus::Migrated
            } else {
                LayoutStatus::Ready
            },
            warnings,
        })
    }

    /// Add moves for entries that a previous version of this code did not know
    /// about, without disturbing the journal's ordering for existing entries.
    fn extend_planned(&self, moves: &mut Vec<MoveRecord>) -> Result<()> {
        let planned = self.plan_moves()?;
        for move_ in planned {
            if !moves
                .iter()
                .any(|existing| existing.from == move_.from && existing.to == move_.to)
            {
                moves.push(move_);
            }
        }
        Ok(())
    }

    fn plan_moves(&self) -> Result<Vec<MoveRecord>> {
        let mut moves = Vec::new();
        for entry in STATIC_MOVES {
            if entry.from == "tasks" {
                // `tasks/` is also the new category directory. Only a v1
                // directory holding session task databases is migrated.
                if !has_legacy_tasks(&self.root) {
                    continue;
                }
            } else if !self.root.join(entry.from).try_exists()? {
                continue;
            }
            moves.push(MoveRecord::pending(entry.from, entry.to));
        }
        let mut backups = Vec::new();
        let mut debug_logs = Vec::new();
        if let Ok(entries) = fs::read_dir(&self.root) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with("config.json.bak-") {
                    backups.push(name);
                } else if name.starts_with("debug-") && name.ends_with(".log") {
                    debug_logs.push(name);
                }
            }
        }
        backups.sort();
        debug_logs.sort();
        for name in backups {
            moves.push(MoveRecord::pending(
                &name,
                format!("{CONFIG_DIR}/backups/{name}"),
            ));
        }
        for name in debug_logs {
            moves.push(MoveRecord::pending(
                &name,
                format!("{DIAGNOSTICS_DIR}/debug/{name}"),
            ));
        }
        Ok(moves)
    }

    /// Names that this version knows how to migrate but that still exist at the
    /// root. After `layout.json` is ready these were written by an older binary.
    fn legacy_entries(&self) -> Result<Vec<String>> {
        let mut warnings = Vec::new();
        for entry in STATIC_MOVES {
            let is_legacy = if entry.from == "tasks" {
                has_legacy_tasks(&self.root)
            } else {
                self.root.join(entry.from).try_exists()?
            };
            if is_legacy {
                warnings.push(entry.from.to_owned());
            }
        }
        if let Ok(entries) = fs::read_dir(&self.root) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if (name.starts_with("config.json.bak-")
                    || (name.starts_with("debug-") && name.ends_with(".log")))
                    && !warnings.contains(&name)
                {
                    warnings.push(name);
                }
            }
        }
        Ok(warnings)
    }

    fn legacy_warnings(&self) -> Result<Vec<String>> {
        let mut warnings = self.legacy_entries()?;
        warnings.extend(self.unexpected_root_entries()?);
        warnings.sort();
        warnings.dedup();
        Ok(warnings)
    }

    /// Read-only layout diagnostics for `/doctor`; never migrates or writes.
    pub fn diagnostics(&self) -> Result<LayoutDiagnostics> {
        let layout = read_layout(&self.root)?;
        let mut legacy_entries = if self.root.try_exists()? {
            self.legacy_entries()?
        } else {
            Vec::new()
        };
        legacy_entries.sort();
        legacy_entries.dedup();
        let unexpected_entries = if self.root.try_exists()? {
            self.unexpected_root_entries()?
        } else {
            Vec::new()
        };
        Ok(LayoutDiagnostics {
            version: layout.as_ref().map(|layout| layout.version),
            state: layout.as_ref().map(|layout| match layout.state {
                LayoutStateName::Migrating => "migrating",
                LayoutStateName::Ready => "ready",
            }),
            legacy_entries,
            unexpected_entries,
        })
    }

    fn final_warnings(&self) -> Result<Vec<String>> {
        let warnings = self.legacy_warnings()?;
        if !warnings.is_empty() {
            tracing::warn!(
                module = "DataLayout",
                root = %self.root.display(),
                entries = %warnings.join(", "),
                "Unknown or legacy entries remain in the data directory root"
            );
        }
        Ok(warnings)
    }

    /// Root entries that are neither allowlisted nor known legacy names.
    pub fn unexpected_root_entries(&self) -> Result<Vec<String>> {
        let mut entries = Vec::new();
        for entry in fs::read_dir(&self.root)
            .with_context(|| format!("Reading data directory {}", self.root.display()))?
        {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if ROOT_ALLOWLIST.contains(&name.as_str())
                || IGNORED_ROOT_ENTRIES.contains(&name.as_str())
                || is_known_legacy_name(&name)
            {
                continue;
            }
            entries.push(name);
        }
        entries.sort();
        Ok(entries)
    }

    /// True when any registered lock file is held by another process.
    fn active_instance(&self) -> Result<Option<PathBuf>> {
        let locks = |layout: &DataLayout| {
            [
                layout.lock_path("session").join(".directory"),
                layout.lock_path("session").join(".orphan-sweep"),
                layout.lock_path("session-message").join(".directory"),
            ]
            .into_iter()
        };
        let legacy = [
            self.root.join("session-locks").join(".directory"),
            self.root.join("session-locks").join(".orphan-sweep"),
            self.root.join("session-message-locks").join(".directory"),
        ];
        for path in locks(self).chain(legacy) {
            if locked_by_other(&path)? {
                return Ok(Some(path));
            }
        }
        for (old, new) in [
            ("workspace-locks", "state/locks/workspace"),
            ("session-presence", "state/presence"),
            ("session-execution-locks", "state/locks/session-execution"),
        ] {
            for directory in [self.root.join(old), self.root.join(new)] {
                if let Some(path) = locked_in_directory(&directory)? {
                    return Ok(Some(path));
                }
            }
        }
        for directory in [self.root.join("request-records"), self.request_records()] {
            if let Some(path) = locked_request_records(&directory)? {
                return Ok(Some(path));
            }
        }
        Ok(None)
    }
}

struct MigrationLock {
    file: Option<File>,
    path: PathBuf,
}

impl Drop for MigrationLock {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            let _ = file.unlock();
        }
        // Waiting processes hold their own descriptor and re-read layout.json
        // after acquiring it; a later process sees the ready state.
        let _ = fs::remove_file(&self.path);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum MoveStatus {
    Pending,
    Done,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MoveRecord {
    from: String,
    to: String,
    status: MoveStatus,
}

impl MoveRecord {
    fn pending(from: impl Into<String>, to: impl Into<String>) -> Self {
        Self {
            from: from.into(),
            to: to.into(),
            status: MoveStatus::Pending,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LayoutStateName {
    Migrating,
    Ready,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LayoutFile {
    version: u32,
    state: LayoutStateName,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    migrated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    from_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    moves: Vec<MoveRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
}

fn read_layout(root: &Path) -> Result<Option<LayoutFile>> {
    let path = root.join(LAYOUT_FILE);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("Reading {}", path.display())),
    };
    serde_json::from_slice(&bytes)
        .with_context(|| format!("Invalid layout file {}", path.display()))
        .map(Some)
}

fn write_layout(root: &Path, layout: &LayoutFile) -> Result<()> {
    let destination = root.join(LAYOUT_FILE);
    let temporary = root.join(format!(".{LAYOUT_FILE}.{}.tmp", uuid::Uuid::new_v4()));
    let bytes = serde_json::to_vec_pretty(layout)?;
    let result = (|| -> Result<()> {
        let mut file = File::options()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(&bytes)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        replace_file(&temporary, &destination)?;
        sync_dir(root)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.with_context(|| format!("Writing {}", destination.display()))
}

fn now() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

/// Recover the data root from an application config path.
///
/// Callers historically passed `<data>/config.json` around as a proxy for the
/// data directory. Configuration now lives in `<data>/config/`, so a bare
/// `parent()` would return the category directory instead of the root.
pub fn data_root_of(config_path: &Path) -> Option<&Path> {
    let parent = config_path.parent()?;
    if parent.file_name() == Some(std::ffi::OsStr::new(CONFIG_DIR)) {
        parent.parent()
    } else {
        Some(parent)
    }
}

/// Write a configuration file, creating `config/` on first use.
///
/// Category directories are not created by migration alone, so every writer
/// goes through one helper instead of assuming the parent exists.
pub fn write_config_file(
    path: impl AsRef<Path>,
    contents: impl AsRef<[u8]>,
) -> std::io::Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, contents)
}

fn readme() -> String {
    let mut text = String::from(
        "# DeepSeek Code data directory\n\n\
         This directory is shared by the CLI and Desktop. Everything is grouped by \
         lifetime: configuration, irreplaceable data, runtime state, resumable tasks, \
         cache, diagnostics and managed resources.\n\n\
         | Directory | Purpose | Deletable | Back up |\n\
         |---|---|---|---|\n\
         | `config/` | Human-edited configuration and credentials | Reconfigure | Yes |\n\
         | `data/` | Sessions, memory, workspaces, images: cannot be regenerated | No | Yes |\n\
         | `state/` | Locks and process coordination: rebuilt on restart | Yes, while stopped | No |\n\
         | `tasks/` | Background jobs, plans, checkpoints, recovery points | Loses resumability | Recommended |\n\
         | `cache/` | Derived files regenerated on next use | Yes | No |\n\
         | `diagnostics/` | Request records, logs, debug traces | Yes | As needed |\n\
         | `resources/` | Managed language-server installs | Yes, reinstall | No |\n\n\
         Root anchors: `global.db` (+ SQLite sidecars), `layout.json`, `AGENTS.md`, \
         `CLAUDE.md`, `skills/`, `mcp-servers.json`, `mcp-servers.lock`.\n\n\
         New entries belong in one of the seven categories and must be registered in \
         `crates/core/src/data_layout.rs`.\n\n\
         To back up, stop every CLI and Desktop instance and copy this whole directory. \
         If `layout.json` is missing, the next start migrates a v1 directory in place.\n",
    );
    // Keep the table readable in terminals as well as editors.
    text.push_str("Do not remove `global.db` or `data/` while sessions are open.\n");
    text
}

fn write_readme(root: &Path) -> Result<()> {
    let path = root.join(README_FILE);
    if path.try_exists()? {
        return Ok(());
    }
    let temporary = root.join(format!(".{README_FILE}.{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = File::options()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(readme().as_bytes())?;
        file.sync_all()?;
        rename_no_replace(&temporary, &path)?;
        sync_dir(root)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.with_context(|| format!("Writing {}", path.display()))
}

struct MoveSpec {
    from: &'static str,
    to: &'static str,
}

/// Order is deliberate: cache and diagnostics can be lost, tasks are resumable,
/// then runtime state (locks last), managed resources and configuration.
/// Truth data is last, with `sessions` at the very end.
///
/// The old `tasks/` directory collides with the new `tasks/` category. It is
/// moved to `data/session-tasks` before anything is placed under `tasks/`.
static STATIC_MOVES: &[MoveSpec] = &[
    // Cache.
    MoveSpec {
        from: "tool-outputs",
        to: "cache/tool-outputs",
    },
    MoveSpec {
        from: "tool-output-metadata",
        to: "cache/tool-output-metadata",
    },
    MoveSpec {
        from: "mcp-tool-snapshots",
        to: "cache/mcp-tool-snapshots",
    },
    MoveSpec {
        from: "environment",
        to: "cache/environment",
    },
    MoveSpec {
        from: "git-status",
        to: "cache/git-status",
    },
    MoveSpec {
        from: "file-state",
        to: "cache/file-state",
    },
    MoveSpec {
        from: "compactions",
        to: "cache/compactions",
    },
    MoveSpec {
        from: "screenshots",
        to: "cache/screenshots",
    },
    MoveSpec {
        from: "updates",
        to: "cache/updates",
    },
    MoveSpec {
        from: "plugins",
        to: "cache/plugins",
    },
    // Diagnostics.
    MoveSpec {
        from: "request-records",
        to: "diagnostics/request-records",
    },
    MoveSpec {
        from: "logs",
        to: "diagnostics/logs",
    },
    MoveSpec {
        from: "design-conversions",
        to: "diagnostics/design-conversions",
    },
    MoveSpec {
        from: "hook-transcripts",
        to: "diagnostics/hook-transcripts",
    },
    // The old `tasks/` directory (per-session task databases) must move before
    // the new `tasks/` category is created.
    MoveSpec {
        from: "tasks",
        to: "data/session-tasks",
    },
    // Resumable tasks and plans.
    MoveSpec {
        from: "background-tasks",
        to: "tasks/background",
    },
    MoveSpec {
        from: "shell-tasks",
        to: "tasks/shell",
    },
    MoveSpec {
        from: "detached-shells",
        to: "tasks/detached-shells",
    },
    MoveSpec {
        from: "checkpoints",
        to: "tasks/checkpoints",
    },
    MoveSpec {
        from: "plans",
        to: "tasks/plans",
    },
    MoveSpec {
        from: "turn-context",
        to: "tasks/turn-context",
    },
    MoveSpec {
        from: "design-plans",
        to: "tasks/design-plans",
    },
    MoveSpec {
        from: "super-agent",
        to: "tasks/super-agent",
    },
    MoveSpec {
        from: "toolscript-jobs",
        to: "tasks/toolscript-jobs",
    },
    MoveSpec {
        from: "workflows",
        to: "tasks/workflows",
    },
    MoveSpec {
        from: "workplans",
        to: "tasks/workplans",
    },
    MoveSpec {
        from: "wakeups",
        to: "tasks/wakeups",
    },
    // Runtime state (locks last).
    MoveSpec {
        from: ".session-bootstrap",
        to: "state/bootstrap",
    },
    MoveSpec {
        from: ".global-staging",
        to: "state/staging",
    },
    MoveSpec {
        from: "turn-completions",
        to: "state/turn-completions",
    },
    MoveSpec {
        from: "shell-cwd",
        to: "state/shell-cwd",
    },
    MoveSpec {
        from: "session-env",
        to: "state/session-env",
    },
    MoveSpec {
        from: "session-presence",
        to: "state/presence",
    },
    MoveSpec {
        from: "session-workers",
        to: "state/workers",
    },
    MoveSpec {
        from: "hook-once",
        to: "state/hook-once",
    },
    MoveSpec {
        from: "plugin-runs",
        to: "state/plugin-runs",
    },
    MoveSpec {
        from: "skill-runs",
        to: "state/skill-runs",
    },
    MoveSpec {
        from: "cron-locks",
        to: "state/locks/cron",
    },
    MoveSpec {
        from: "server-token",
        to: "state/server-token",
    },
    MoveSpec {
        from: "dev-bridge-key",
        to: "state/dev-bridge-key",
    },
    MoveSpec {
        from: "session-locks",
        to: "state/locks/session",
    },
    MoveSpec {
        from: "workspace-locks",
        to: "state/locks/workspace",
    },
    MoveSpec {
        from: "session-message-locks",
        to: "state/locks/session-message",
    },
    MoveSpec {
        from: "session-execution-locks",
        to: "state/locks/session-execution",
    },
    MoveSpec {
        from: "background-task-publication.lock",
        to: "state/locks/publication.lock",
    },
    MoveSpec {
        from: ".migrated",
        to: "state/migrated",
    },
    // Managed resources.
    MoveSpec {
        from: "lsp",
        to: "resources/lsp",
    },
    MoveSpec {
        from: "linux",
        to: "resources/linux",
    },
    // Configuration.
    MoveSpec {
        from: "config.json",
        to: "config/config.json",
    },
    MoveSpec {
        from: "settings.json",
        to: "config/settings.json",
    },
    MoveSpec {
        from: "customization.json",
        to: "config/customization.json",
    },
    MoveSpec {
        from: "startup-prompts.json",
        to: "config/startup-prompts.json",
    },
    MoveSpec {
        from: "tips.json",
        to: "config/tips.json",
    },
    MoveSpec {
        from: "keybindings.json",
        to: "config/keybindings.json",
    },
    MoveSpec {
        from: "skill-states.json",
        to: "config/skill-states.json",
    },
    MoveSpec {
        from: "scroll-speed",
        to: "config/scroll-speed",
    },
    MoveSpec {
        from: "mcp-states.json",
        to: "config/mcp-states.json",
    },
    MoveSpec {
        from: "agent-models.json",
        to: "config/agent-models.json",
    },
    MoveSpec {
        from: "remote-control.json",
        to: "config/remote-control.json",
    },
    MoveSpec {
        from: "github-token.json",
        to: "config/github-token.json",
    },
    MoveSpec {
        from: "cli.json",
        to: "config/cli.json",
    },
    MoveSpec {
        from: "providers.json",
        to: "config/providers.json",
    },
    MoveSpec {
        from: "statusline.json",
        to: "config/statusline.json",
    },
    MoveSpec {
        from: "composio_config.json",
        to: "config/composio_config.json",
    },
    MoveSpec {
        from: "orchestrator.toml",
        to: "config/orchestrator.toml",
    },
    MoveSpec {
        from: "anthropic-oauth.dat",
        to: "config/anthropic-oauth.dat",
    },
    MoveSpec {
        from: "anthropic-oauth.lock",
        to: "config/anthropic-oauth.lock",
    },
    // Truth data.
    MoveSpec {
        from: "global-blobs",
        to: "data/global-blobs",
    },
    MoveSpec {
        from: "workspaces",
        to: "data/workspaces",
    },
    MoveSpec {
        from: "pasted-images",
        to: "data/pasted-images",
    },
    MoveSpec {
        from: "memory.sqlite3",
        to: "data/memory.sqlite3",
    },
    MoveSpec {
        from: "team-server.sqlite3",
        to: "data/team-server.sqlite3",
    },
    MoveSpec {
        from: "agent-messages.sqlite3",
        to: "data/agent-messages.sqlite3",
    },
    MoveSpec {
        from: "agent-memory",
        to: "data/agent-memory",
    },
    MoveSpec {
        from: "memory",
        to: "data/memory",
    },
    MoveSpec {
        from: "teams",
        to: "data/teams",
    },
    MoveSpec {
        from: "schedules.sqlite3",
        to: "data/schedules.sqlite3",
    },
    MoveSpec {
        from: "costs",
        to: "data/costs",
    },
    MoveSpec {
        from: "knowledge",
        to: "data/knowledge",
    },
    MoveSpec {
        from: "analytics",
        to: "data/analytics",
    },
    MoveSpec {
        from: "uploads",
        to: "data/uploads",
    },
    MoveSpec {
        from: "app-studio",
        to: "data/app-studio",
    },
    MoveSpec {
        from: "composio_sessions.json",
        to: "data/composio_sessions.json",
    },
    MoveSpec {
        from: "backups",
        to: "data/backups",
    },
    MoveSpec {
        from: "conversations",
        to: "data/legacy/conversations",
    },
    MoveSpec {
        from: "claude-desktop.json",
        to: "data/legacy/claude-desktop.json",
    },
    // Sessions last: everything above can fail without hiding live history.
    MoveSpec {
        from: "sessions",
        to: "data/sessions",
    },
];

/// True only for a v1 `tasks/` directory: per-session task databases live
/// directly below it. The v2 `tasks/` category holds feature subdirectories.
fn has_legacy_tasks(root: &Path) -> bool {
    let Ok(entries) = fs::read_dir(root.join(TASKS_DIR)) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        path.extension()
            .is_some_and(|extension| extension == "sqlite3")
            || path.file_name() == Some(std::ffi::OsStr::new("projects"))
    })
}

fn is_known_legacy_name(name: &str) -> bool {
    STATIC_MOVES.iter().any(|entry| entry.from == name)
        || name.starts_with("config.json.bak-")
        || (name.starts_with("debug-") && name.ends_with(".log"))
}

fn locked_by_other(path: &Path) -> Result<bool> {
    let file = match File::options().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
            ) =>
        {
            return Ok(false);
        }
        Err(error) => {
            return Err(error).with_context(|| format!("Probing lock {}", path.display()))
        }
    };
    match file.try_lock() {
        Ok(()) => {
            let _ = file.unlock();
            Ok(false)
        }
        Err(fs::TryLockError::WouldBlock) => Ok(true),
        Err(fs::TryLockError::Error(error)) => {
            Err(error).with_context(|| format!("Probing lock {}", path.display()))
        }
    }
}

fn locked_in_directory(directory: &Path) -> Result<Option<PathBuf>> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Reading lock directory {}", directory.display()))
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file() && locked_by_other(&path)? {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

fn locked_request_records(directory: &Path) -> Result<Option<PathBuf>> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("Reading request-record directory {}", directory.display())
            })
        }
    };
    for entry in entries.flatten() {
        let path = entry.path().join(".active");
        if path.is_file() && locked_by_other(&path)? {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

fn sync_dir(directory: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(directory)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = directory;
    Ok(())
}

fn rename_with_retry(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut last = None;
    for attempt in 0..3 {
        match rename_no_replace(from, to) {
            Ok(()) => {
                sync_dir(to.parent().unwrap_or(Path::new("."))).map_err(std::io::Error::other)?;
                return Ok(());
            }
            Err(error) => {
                last = Some(error);
                if attempt < 2 {
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("rename failed")))
}

#[cfg(not(windows))]
fn rename_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::rename(from, to)
}

#[cfg(windows)]
fn rename_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    move_file(from, to, 8)
}

#[cfg(not(windows))]
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::rename(from, to)
}

#[cfg(windows)]
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    move_file(from, to, 1 | 8)
}

#[cfg(windows)]
fn move_file(from: &Path, to: &Path, flags: u32) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    extern "system" {
        fn MoveFileExW(old: *const u16, new: *const u16, flags: u32) -> i32;
    }
    let wide = |path: &Path| {
        path.as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect::<Vec<_>>()
    };
    let (source, destination) = (wide(from), wide(to));
    if unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), flags) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(root: &Path) -> DataLayout {
        DataLayout::from_root(&root)
    }

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"fixture").unwrap();
    }

    fn create_legacy_root(root: &Path) {
        touch(&root.join("config.json"));
        touch(&root.join("settings.json"));
        touch(&root.join("skill-states.json"));
        touch(&root.join("scroll-speed"));
        touch(&root.join("mcp-states.json"));
        touch(&root.join("agent-models.json"));
        touch(&root.join("remote-control.json"));
        touch(&root.join("github-token.json"));
        touch(&root.join("config.json.bak-20260101"));
        touch(&root.join("debug-11111111-1111-1111-1111-111111111111.log"));
        touch(&root.join("global.db"));
        touch(&root.join("AGENTS.md"));
        touch(&root.join("tasks/session.sqlite3"));
        for directory in [
            "sessions",
            "global-blobs",
            "workspaces",
            "background-tasks",
            "shell-tasks",
            "checkpoints",
            "plans",
            "turn-context",
            "tool-outputs",
            "environment",
            "request-records",
            "logs",
            "session-locks",
            "lsp",
            "linux",
            "plugins",
            "hook-once",
            "plugin-runs",
            "skill-runs",
        ] {
            fs::create_dir_all(root.join(directory)).unwrap();
            fs::write(root.join(directory).join("fixture"), b"fixture").unwrap();
        }
    }

    #[test]
    fn fresh_root_gets_layout_file_and_readme() {
        let dir = tempfile::tempdir().unwrap();
        let state = layout(dir.path()).migrate_if_needed().unwrap();
        assert_eq!(state.status, LayoutStatus::Fresh);
        assert!(dir.path().join("layout.json").is_file());
        assert!(dir.path().join("README.md").is_file());
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.path().join("layout.json")).unwrap()).unwrap();
        assert_eq!(value["version"], 2);
        assert_eq!(value["state"], "ready");
    }

    #[test]
    fn migrates_every_known_entry() {
        let dir = tempfile::tempdir().unwrap();
        create_legacy_root(dir.path());
        let state = layout(dir.path()).migrate_if_needed().unwrap();
        assert_eq!(state.status, LayoutStatus::Migrated);
        for (source, target) in [
            ("sessions", "data/sessions"),
            ("global-blobs", "data/global-blobs"),
            ("background-tasks", "tasks/background"),
            ("shell-tasks", "tasks/shell"),
            ("checkpoints", "tasks/checkpoints"),
            ("plans", "tasks/plans"),
            ("tool-outputs", "cache/tool-outputs"),
            ("environment", "cache/environment"),
            ("request-records", "diagnostics/request-records"),
            ("logs", "diagnostics/logs"),
            ("session-locks", "state/locks/session"),
            ("config.json", "config/config.json"),
            ("settings.json", "config/settings.json"),
            ("skill-states.json", "config/skill-states.json"),
            ("scroll-speed", "config/scroll-speed"),
            ("mcp-states.json", "config/mcp-states.json"),
            ("agent-models.json", "config/agent-models.json"),
            ("remote-control.json", "config/remote-control.json"),
            ("github-token.json", "config/github-token.json"),
            ("plugins", "cache/plugins"),
            ("hook-once", "state/hook-once"),
            ("plugin-runs", "state/plugin-runs"),
            ("skill-runs", "state/skill-runs"),
            (
                "config.json.bak-20260101",
                "config/backups/config.json.bak-20260101",
            ),
            (
                "debug-11111111-1111-1111-1111-111111111111.log",
                "diagnostics/debug/debug-11111111-1111-1111-1111-111111111111.log",
            ),
            ("lsp", "resources/lsp"),
            ("linux", "resources/linux"),
        ] {
            assert!(!dir.path().join(source).exists(), "source {source} remains");
            assert!(dir.path().join(target).exists(), "target {target} missing");
        }
        // The old `tasks/` moved away; the same path is now the v2 category.
        assert!(dir
            .path()
            .join("data/session-tasks/session.sqlite3")
            .is_file());
        assert!(dir.path().join("tasks/background/fixture").is_file());
        assert!(dir.path().join("global.db").is_file());
        assert!(dir.path().join("AGENTS.md").is_file());
    }

    #[test]
    fn migration_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        create_legacy_root(dir.path());
        layout(dir.path()).migrate_if_needed().unwrap();
        let before = fs::read(dir.path().join("layout.json")).unwrap();
        let state = layout(dir.path()).migrate_if_needed().unwrap();
        assert_eq!(state.status, LayoutStatus::Ready);
        let after = fs::read(dir.path().join("layout.json")).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn resumes_pending_move_after_crash() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("sessions/fixture"));
        touch(&dir.path().join("config.json"));
        let journal = LayoutFile {
            version: LAYOUT_VERSION,
            state: LayoutStateName::Migrating,
            started_at: Some(now()),
            migrated_at: None,
            from_version: Some(1),
            moves: vec![
                MoveRecord {
                    from: "config.json".into(),
                    to: "config/config.json".into(),
                    status: MoveStatus::Done,
                },
                MoveRecord::pending("sessions", "data/sessions"),
            ],
            warnings: Vec::new(),
        };
        write_layout(dir.path(), &journal).unwrap();
        let state = layout(dir.path()).migrate_if_needed().unwrap();
        assert_eq!(state.status, LayoutStatus::Migrated);
        assert!(dir.path().join("data/sessions/fixture").is_file());
    }

    #[test]
    fn treats_missing_source_with_existing_target_as_done() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("data/sessions/fixture"));
        let journal = LayoutFile {
            version: LAYOUT_VERSION,
            state: LayoutStateName::Migrating,
            started_at: Some(now()),
            migrated_at: None,
            from_version: Some(1),
            moves: vec![MoveRecord::pending("sessions", "data/sessions")],
            warnings: Vec::new(),
        };
        write_layout(dir.path(), &journal).unwrap();
        let state = layout(dir.path()).migrate_if_needed().unwrap();
        assert_eq!(state.status, LayoutStatus::Migrated);
        assert!(dir.path().join("data/sessions/fixture").is_file());
    }

    #[test]
    fn refuses_source_and_target_conflict() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("sessions/a"));
        touch(&dir.path().join("data/sessions/b"));
        let error = layout(dir.path()).migrate_if_needed().unwrap_err();
        assert!(error.to_string().contains("Layout conflict"), "{error}");
        assert!(dir.path().join("sessions/a").is_file());
        assert!(dir.path().join("data/sessions/b").is_file());
    }

    #[test]
    fn leaves_unknown_root_entries_and_reports_them() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("my-script.sh"));
        let state = layout(dir.path()).migrate_if_needed().unwrap();
        assert!(dir.path().join("my-script.sh").is_file());
        assert!(state.warnings.iter().any(|entry| entry == "my-script.sh"));
    }

    #[test]
    fn moves_config_backups_to_config_backups() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("config.json.bak-1"));
        touch(&dir.path().join("config.json.bak-2"));
        layout(dir.path()).migrate_if_needed().unwrap();
        assert!(dir
            .path()
            .join("config/backups/config.json.bak-1")
            .is_file());
        assert!(dir
            .path()
            .join("config/backups/config.json.bak-2")
            .is_file());
    }

    #[test]
    fn rejects_newer_layout_version() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("layout.json"),
            br#"{"version": 99, "state": "ready"}"#,
        )
        .unwrap();
        let error = layout(dir.path()).migrate_if_needed().unwrap_err();
        assert!(error.to_string().contains("newer version"), "{error}");
    }

    #[test]
    fn diagnostics_reports_version_and_leftovers() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            layout(dir.path()).diagnostics().unwrap(),
            LayoutDiagnostics {
                version: None,
                state: None,
                legacy_entries: Vec::new(),
                unexpected_entries: Vec::new(),
            }
        );
        layout(dir.path()).migrate_if_needed().unwrap();
        assert_eq!(
            layout(dir.path()).diagnostics().unwrap(),
            LayoutDiagnostics {
                version: Some(LAYOUT_VERSION),
                state: Some("ready"),
                legacy_entries: Vec::new(),
                unexpected_entries: Vec::new(),
            }
        );
        touch(&dir.path().join("sessions/fixture"));
        touch(&dir.path().join("unexpected.txt"));
        let diagnostics = layout(dir.path()).diagnostics().unwrap();
        assert_eq!(diagnostics.version, Some(LAYOUT_VERSION));
        assert_eq!(diagnostics.legacy_entries, ["sessions"]);
        assert_eq!(diagnostics.unexpected_entries, ["unexpected.txt"]);
    }

    #[test]
    fn does_not_create_empty_category_dirs() {
        let dir = tempfile::tempdir().unwrap();
        layout(dir.path()).migrate_if_needed().unwrap();
        for category in CATEGORY_DIRS {
            assert!(
                !dir.path().join(category).exists(),
                "empty category {category} was created"
            );
        }
    }

    #[test]
    fn global_db_stays_at_root() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("global.db"));
        layout(dir.path()).migrate_if_needed().unwrap();
        assert!(dir.path().join("global.db").is_file());
        assert!(!dir.path().join("data/global.db").exists());
    }

    #[test]
    fn refuses_when_a_live_lock_is_held() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("sessions/fixture"));
        let lock_path = dir.path().join("session-locks/.directory");
        touch(&lock_path);
        let lock = File::options()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        crate::fs::lock_file(&lock).unwrap();
        let error = layout(dir.path()).migrate_if_needed().unwrap_err();
        assert!(
            error.to_string().contains("Another DeepSeek Code"),
            "{error}"
        );
        assert!(dir.path().join("sessions/fixture").is_file());
        crate::fs::unlock_file(&lock).unwrap();
    }

    #[test]
    fn accessors_stay_within_their_category() {
        let root = Path::new("/tmp/data-layout-accessors");
        let layout = DataLayout::new(root.to_path_buf());
        assert_eq!(
            layout.config_file("settings.json"),
            root.join("config/settings.json")
        );
        assert_eq!(layout.sessions(), root.join("data/sessions"));
        assert_eq!(layout.background_tasks(), root.join("tasks/background"));
        assert_eq!(
            layout.request_records(),
            root.join("diagnostics/request-records")
        );
        assert_eq!(
            layout.lock_path("session"),
            root.join("state/locks/session")
        );
        assert_eq!(layout.lsp(), root.join("resources/lsp"));
        assert_eq!(layout.linux_rootfs(), root.join("resources/linux/rootfs"));
        assert_eq!(
            layout.mcp_states(),
            root.join("config/mcp-states.json")
        );
        assert_eq!(layout.hook_once(), root.join("state/hook-once"));
        assert_eq!(layout.plugins_cache(), root.join("cache/plugins"));
        assert_eq!(layout.skills(), root.join("skills"));
        assert_eq!(layout.global_db(), root.join("global.db"));
    }

    #[test]
    fn readme_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("README.md"), "user readme").unwrap();
        layout(dir.path()).migrate_if_needed().unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("README.md")).unwrap(),
            "user readme"
        );
    }

    #[test]
    fn temporary_layout_file_is_removed() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("sessions/fixture"));
        layout(dir.path()).migrate_if_needed().unwrap();
        for entry in fs::read_dir(dir.path()).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            assert!(!name.ends_with(".tmp"), "temporary file {name} remains");
        }
    }

    #[test]
    fn lock_file_is_removed_after_migration() {
        let dir = tempfile::tempdir().unwrap();
        layout(dir.path()).migrate_if_needed().unwrap();
        assert!(!dir.path().join(LAYOUT_LOCK).exists());
    }

    #[test]
    fn debug_logs_and_unknown_files_are_ordered_deterministically() {
        let dir = tempfile::tempdir().unwrap();
        touch(&dir.path().join("debug-b.log"));
        touch(&dir.path().join("debug-a.log"));
        layout(dir.path()).migrate_if_needed().unwrap();
        assert!(dir.path().join("diagnostics/debug/debug-a.log").is_file());
        assert!(dir.path().join("diagnostics/debug/debug-b.log").is_file());
    }

    #[test]
    fn write_layout_replaces_an_existing_file_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let state = LayoutFile {
            version: LAYOUT_VERSION,
            state: LayoutStateName::Ready,
            started_at: None,
            migrated_at: Some(now()),
            from_version: None,
            moves: Vec::new(),
            warnings: Vec::new(),
        };
        write_layout(dir.path(), &state).unwrap();
        let mut replacement = state.clone();
        replacement.migrated_at = Some("second".into());
        write_layout(dir.path(), &replacement).unwrap();
        let parsed = read_layout(dir.path()).unwrap().unwrap();
        assert_eq!(parsed.migrated_at.as_deref(), Some("second"));
    }
}
