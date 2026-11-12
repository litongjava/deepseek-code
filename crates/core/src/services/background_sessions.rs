//! Detached CLI jobs with an owning supervisor, durable results and liveness leases.
//! Credentials are inherited through the environment, never serialized in job files.
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub const WORKER_ENV: &str = "DEEPSEEK_CODE_BACKGROUND_JOB";
pub const CONTROL_ENV: &str = "DEEPSEEK_CODE_BACKGROUND_CONTROL";
/// The supervising worker's model snapshot and auto-route decision. They belong
/// to that worker alone: a managed child resolves its own model from its launch
/// arguments, and a deliberately forwarded snapshot arrives through the
/// explicit `environment` after inherited values have been stripped.
pub const WORKER_MODEL_ENV: &str = "DEEPSEEK_CODE_WORKER_MODEL";
pub const WORKER_AUTO_ROUTE_ENV: &str = "DEEPSEEK_CODE_WORKER_AUTO_ROUTE";
/// The exclusive-execution token of a reserved job. The host publishes it in
/// the worker claim before spawning and passes it to that supervisor only, so
/// a duplicate supervisor cannot execute the same job.
pub const WORKER_TOKEN_ENV: &str = "DEEPSEEK_CODE_BACKGROUND_TOKEN";

/// Host-private variables of the supervising worker. An ordinary child process
/// (shell command, hook, MCP server, nested CLI) is a separate session:
/// inheriting these would make it claim the worker's control directory and be
/// rejected by its own ownership check, or silently join the parent's job. A
/// managed worker additionally inherits the parent's model snapshot, which its
/// own launch then rejects as a mismatch.
pub const INTERNAL_ENV: [&str; 5] = [
    WORKER_ENV,
    CONTROL_ENV,
    WORKER_MODEL_ENV,
    WORKER_AUTO_ROUTE_ENV,
    WORKER_TOKEN_ENV,
];

/// A command type whose environment can be pruned before it starts.
pub trait EnvScrub {
    fn scrub_env(&mut self, name: &str);
}

impl EnvScrub for std::process::Command {
    fn scrub_env(&mut self, name: &str) {
        self.env_remove(name);
    }
}

impl EnvScrub for tokio::process::Command {
    fn scrub_env(&mut self, name: &str) {
        self.env_remove(name);
    }
}

/// Remove the host-private variables from a command about to start an ordinary
/// child process. The supervisor that starts a managed worker passes its own
/// control directory explicitly, so it is unaffected.
pub fn strip_internal_env<C: EnvScrub>(command: &mut C) {
    for name in INTERNAL_ENV {
        command.scrub_env(name);
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: String,
    #[serde(default)]
    pub foreground: bool,
    pub parent_session_id: String,
    pub session_id: String,
    pub title: String,
    pub status: String,
    pub supervisor_pid: Option<u32>,
    pub child_pid: Option<u32>,
    pub started_ms: u64,
    pub heartbeat_ms: u64,
    pub finished_ms: Option<u64>,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
    /// True when this job has durable authorization to execute: a worker claim
    /// was published or the host recorded a spawn fact. A reserved job publishes
    /// its claim **before** `Command::spawn`, so a crash in the spawn window also
    /// reports true and a later host must adopt rather than spawn again. This is
    /// deliberately not "a process definitely ran": a reserved job can be
    /// authorized and still lack a process, and a non-reserved job's claim is
    /// published by its worker after it starts. `false` only means no
    /// authorization was observed when this record was read.
    #[serde(default)]
    pub launched: bool,
}

/// Durable view of a reserved job, used by hosts that reconcile after a crash.
#[derive(Debug, Clone)]
pub enum JobSnapshot {
    /// No directory or no published state: the job never started.
    Missing,
    /// The published state, with the same stale-heartbeat observation as `list`.
    Present(Job),
    /// The state file exists but cannot be parsed. The original bytes are kept.
    Unreadable(String),
}

/// Reads a reserved job without creating it or scanning by title. The caller
/// supplies the identity it reserved before any external side effect.
pub fn inspect(root: &Path, id: &str) -> Result<JobSnapshot> {
    let dir = directory(root, id)?;
    if !dir.exists() {
        return Ok(JobSnapshot::Missing);
    }
    let path = dir.join("state.json");
    match fs::read(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(JobSnapshot::Missing),
        Err(error) => Err(error).with_context(|| format!("Reading task state {}", path.display())),
        Ok(bytes) => match serde_json::from_slice::<Job>(&bytes) {
            Ok(job) => Ok(JobSnapshot::Present(published(&dir, job)?)),
            Err(error) => Ok(JobSnapshot::Unreadable(error.to_string())),
        },
    }
}

/// The launch arguments published for a reserved job. Reconciliation uses them
/// to prove the job still belongs to the same instruction and workspace.
pub fn launch_of(root: &Path, id: &str) -> Result<Option<Launch>> {
    let path = directory(root, id)?.join("launch.json");
    match fs::read(&path) {
        Ok(bytes) => Ok(Some(
            serde_json::from_slice(&bytes)
                .with_context(|| format!("Parsing task launch {}", path.display()))?,
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("Reading task launch {}", path.display())),
    }
}

/// A concurrent host owns the spawn critical section for this job. The caller
/// must not start a second supervisor and should reconcile on its next pass.
#[derive(Debug)]
pub struct LaunchInProgress;

#[cfg(test)]
thread_local! {
    /// Test-only boundary between a successful `spawn` and the host's own
    /// publication. It either waits for the real supervisor to publish a
    /// terminal state (deterministic publication race) or injects an
    /// interruption (host death before its launch record). Production builds
    /// compile this out.
    static AFTER_SPAWN_TEST_HOOK: std::cell::RefCell<
        Option<Box<dyn FnOnce(&Path) -> Result<()>>>,
    > = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn install_after_spawn_test_hook(
    hook: impl FnOnce(&Path) -> Result<()> + 'static,
) {
    AFTER_SPAWN_TEST_HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
}

#[cfg(test)]
fn after_spawn_test_hook(dir: &Path) -> Result<()> {
    let hook = AFTER_SPAWN_TEST_HOOK.with(|slot| slot.borrow_mut().take());
    match hook {
        Some(hook) => hook(dir),
        None => Ok(()),
    }
}

#[cfg(not(test))]
fn after_spawn_test_hook(_dir: &Path) -> Result<()> {
    Ok(())
}

impl std::fmt::Display for LaunchInProgress {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Another host is already starting this background task"
        )
    }
}

impl std::error::Error for LaunchInProgress {}

pub fn is_launch_in_progress(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| cause.downcast_ref::<LaunchInProgress>().is_some())
}

/// Immutable spawn fact. The host publishes it only after `Command::spawn`
/// succeeded, so its presence proves a supervisor process was started. The
/// worker claim, published earlier, remains the authorization that prevents a
/// second spawn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnFact {
    pub supervisor_pid: u32,
    pub token: String,
    pub published_ms: u64,
}

pub const SPAWN_FACT_FILE: &str = "spawn.json";

fn worker_claim_path(dir: &Path) -> PathBuf {
    dir.join("worker.claim")
}

/// The exclusive-execution token published for this job directory.
fn worker_claim_token(dir: &Path) -> Result<Option<String>> {
    match fs::read_to_string(worker_claim_path(dir)) {
        Ok(token) => Ok(Some(token)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error)
            .with_context(|| format!("Reading worker claim {}", worker_claim_path(dir).display())),
    }
}

/// Publishes the exclusive claim. Returns false when another writer already
/// owns it; the claim is never replaced.
fn create_worker_claim(dir: &Path, token: &str) -> Result<bool> {
    let path = worker_claim_path(dir);
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(mut file) => {
            file.write_all(token.as_bytes())?;
            file.sync_all()?;
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error).with_context(|| format!("Claiming {}", path.display())),
    }
}

/// The immutable spawn fact of a reserved job, if the host recorded it.
pub fn spawn_fact(root: &Path, id: &str) -> Result<Option<SpawnFact>> {
    let path = directory(root, id)?.join(SPAWN_FACT_FILE);
    match fs::read(&path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).with_context(
            || format!("Parsing spawn fact {}", path.display()),
        )?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("Reading spawn fact {}", path.display())),
    }
}

/// Writes the spawn fact exactly once. A retry that finds the same fact is
/// idempotent; a different one means the directory is inconsistent.
fn write_spawn_fact(dir: &Path, fact: &SpawnFact) -> Result<()> {
    let path = dir.join(SPAWN_FACT_FILE);
    let parent = path.parent().context("Missing parent")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut temporary, fact)?;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    for attempt in 0..=20 {
        match temporary.persist_noclobber(&path) {
            Ok(file) => {
                file.sync_all()?;
                #[cfg(unix)]
                fs::File::open(parent)?.sync_all()?;
                return Ok(());
            }
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                let existing: SpawnFact = serde_json::from_slice(&fs::read(&path)?)
                    .with_context(|| format!("Parsing spawn fact {}", path.display()))?;
                ensure!(
                    existing.token == fact.token
                        && existing.supervisor_pid == fact.supervisor_pid,
                    "Spawn fact {} belongs to a different supervisor; refusing to replace it",
                    path.display()
                );
                return Ok(());
            }
            Err(error)
                if error.error.kind() == std::io::ErrorKind::PermissionDenied && attempt < 20 =>
            {
                temporary = error.file;
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(error.error.into()),
        }
    }
    unreachable!()
}

fn same_workspace(left: &Path, right: &Path) -> bool {
    match (left.canonicalize(), right.canonicalize()) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

/// Reads the published lifecycle state and merges the durable publication
/// evidence. Lifecycle fields come only from `state.json`; the worker claim and
/// spawn fact contribute the `launched` authorization flag.
fn published(dir: &Path, mut job: Job) -> Result<Job> {
    if !job.launched {
        job.launched = worker_claim_token(dir)?.is_some() || dir.join(SPAWN_FACT_FILE).exists();
    }
    Ok(observed(job))
}

#[derive(Serialize, Deserialize)]
pub struct Launch {
    pub executable: PathBuf,
    pub arguments: Vec<String>,
    pub cwd: PathBuf,
}

pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
pub(crate) fn save<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut temporary = tempfile::NamedTempFile::new_in(path.parent().context("Missing parent")?)?;
    serde_json::to_writer(&mut temporary, value)?;
    temporary.flush()?;
    // Flush file contents before publishing the name, including after power loss.
    temporary.as_file().sync_all()?;
    // Windows readers/antivirus can briefly deny replacement of a live state
    // file. Retry the same complete temporary file; never expose partial JSON.
    for attempt in 0..=20 {
        match temporary.persist(path) {
            Ok(file) => {
                file.sync_all()?;
                // Persist the renamed directory entry on Unix as well.
                #[cfg(unix)]
                fs::File::open(path.parent().context("Missing parent")?)?.sync_all()?;
                return Ok(());
            }
            Err(error)
                if error.error.kind() == std::io::ErrorKind::PermissionDenied && attempt < 20 =>
            {
                temporary = error.file;
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(error.error.into()),
        }
    }
    unreachable!()
}
pub(crate) fn directory(root: &Path, id: &str) -> Result<PathBuf> {
    ensure!(
        uuid::Uuid::parse_str(id).is_ok(),
        "Invalid background task ID"
    );
    Ok(root.join("background-tasks").join(id))
}
pub(crate) fn read_job(dir: &Path) -> Result<Job> {
    let path = dir.join("state.json");
    let bytes =
        fs::read(&path).with_context(|| format!("Reading task state {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("Parsing task state {}", path.display()))
}
fn observed(mut job: Job) -> Job {
    if matches!(job.status.as_str(), "starting" | "running")
        && now().saturating_sub(job.heartbeat_ms) > 10_000
    {
        job.status = "unresponsive".into();
        job.error = Some("Supervisor heartbeat expired; process completion is unknown".into());
    }
    job
}

pub(crate) struct SessionClaim {
    path: PathBuf,
    id: String,
    pub(crate) committed: bool,
}
impl SessionClaim {
    fn path(root: &Path, session: &str) -> Result<PathBuf> {
        ensure!(
            uuid::Uuid::parse_str(session).is_ok(),
            "Invalid foreground session ID"
        );
        Ok(root
            .join("background-tasks")
            .join(format!("session-{session}.claim")))
    }
    pub(crate) fn acquire(root: &Path, session: &str, id: &str) -> Result<Self> {
        let path = Self::path(root, session)?;
        let mut file = fs::OpenOptions::new().write(true).create_new(true).open(&path)
            .with_context(||format!("Session already has a worker claim at {}. Inspect its job; an unresponsive owner is never replaced automatically.",path.display()))?;
        file.write_all(id.as_bytes())?;
        file.sync_all()?;
        Ok(Self {
            path,
            id: id.into(),
            committed: false,
        })
    }
    pub(crate) fn release(root: &Path, session: &str, id: &str) -> Result<()> {
        let path = Self::path(root, session)?;
        if fs::read_to_string(&path).ok().as_deref() == Some(id) {
            fs::remove_file(path)?;
        }
        Ok(())
    }
}
impl Drop for SessionClaim {
    fn drop(&mut self) {
        if !self.committed
            && fs::read_to_string(&self.path).ok().as_deref() == Some(self.id.as_str())
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Build the command that starts a managed worker. Inherited worker state is
/// removed first, then only what the caller passes explicitly is applied;
/// delegation forwards a deliberate model snapshot that way. A reserved job's
/// exclusive token is passed to this supervisor and no other process.
fn supervisor_command(
    supervisor: &Path,
    dir: &Path,
    environment: &[(String, String)],
    token: Option<&str>,
) -> Command {
    let mut command = Command::new(supervisor);
    strip_internal_env(&mut command);
    command
        .envs(environment.iter().map(|(key, value)| (key, value)))
        .env(WORKER_ENV, dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(token) = token {
        command.env(WORKER_TOKEN_ENV, token);
    }
    command
}

/// Starts another instance of the host executable as an independent supervisor.
/// The host must call run_worker_from_env before parsing its normal CLI arguments.
pub fn spawn_foreground(
    root: &Path,
    supervisor: &Path,
    launch: Launch,
    session: &str,
    title: &str,
    environment: &[(String, String)],
) -> Result<Job> {
    spawn_impl(
        root,
        supervisor,
        launch,
        session,
        session,
        title,
        environment,
        true,
    )
}

pub fn spawn(
    root: &Path,
    supervisor: &Path,
    launch: Launch,
    parent: &str,
    session: &str,
    title: &str,
    environment: &[(String, String)],
) -> Result<Job> {
    spawn_impl(
        root,
        supervisor,
        launch,
        parent,
        session,
        title,
        environment,
        false,
    )
}

fn spawn_impl(
    root: &Path,
    supervisor: &Path,
    launch: Launch,
    parent: &str,
    session: &str,
    title: &str,
    environment: &[(String, String)],
    foreground: bool,
) -> Result<Job> {
    spawn_at(
        root,
        supervisor,
        launch,
        parent,
        session,
        title,
        environment,
        foreground,
        None,
    )
}

/// Starts a job whose UUID was reserved by the caller before any external side
/// effect. A concurrent host that already owns the spawn section receives
/// [`LaunchInProgress`] instead of starting a duplicate supervisor. A crash
/// releases the OS file lock automatically, so the next host can continue the
/// same reserved operation.
pub fn spawn_reserved(
    root: &Path,
    supervisor: &Path,
    launch: Launch,
    parent: &str,
    session: &str,
    title: &str,
    environment: &[(String, String)],
    id: &str,
) -> Result<Job> {
    spawn_at(
        root,
        supervisor,
        launch,
        parent,
        session,
        title,
        environment,
        false,
        Some(id),
    )
}

fn spawn_at(
    root: &Path,
    supervisor: &Path,
    launch: Launch,
    parent: &str,
    session: &str,
    title: &str,
    environment: &[(String, String)],
    foreground: bool,
    reserved: Option<&str>,
) -> Result<Job> {
    let id = match reserved {
        Some(id) => {
            // Validates the reserved identity before touching the filesystem.
            directory(root, id)?;
            id.to_owned()
        }
        None => uuid::Uuid::new_v4().to_string(),
    };
    let dir = directory(root, &id)?;
    fs::create_dir_all(&dir)?;
    // Serialize the spawn decision for a reserved job. The advisory lock is
    // released on process death, which is what makes a crashed host's reserved
    // job recoverable without guessing from PIDs or timestamps.
    let _spawn_lock = match reserved {
        Some(_) => {
            let path = dir.join("spawn.lock");
            let file = fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&path)
                .with_context(|| format!("Opening spawn lock {}", path.display()))?;
            match crate::fs::try_lock_file(&file) {
                Ok(()) => Some(file),
                Err(fs::TryLockError::WouldBlock) => {
                    return Err(anyhow::Error::new(LaunchInProgress));
                }
                Err(fs::TryLockError::Error(error)) => return Err(error.into()),
            }
        }
        None => None,
    };
    let existing = match fs::read(dir.join("state.json")) {
        Ok(bytes) => Some(serde_json::from_slice::<Job>(&bytes).with_context(|| {
            format!("Parsing task state {}", dir.join("state.json").display())
        })?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let claim_token = worker_claim_token(&dir)?;
    let fact_present = dir.join(SPAWN_FACT_FILE).exists();
    let mut job = if let Some(mut job) = existing {
        ensure!(
            reserved.is_some(),
            "Background task {id} already exists at {}",
            dir.display()
        );
        // Identity and launch-contract validation always precede any adoption.
        // A progressed or completed job must never be returned when the caller
        // supplies a different parent, session, instruction or workspace.
        ensure!(
            job.id == id,
            "Reserved background task {id} has a different identity"
        );
        ensure!(
            job.parent_session_id == parent,
            "Reserved background task {id} belongs to another Router session"
        );
        ensure!(
            job.session_id == session,
            "Reserved background task {id} belongs to another child session"
        );
        let stored = launch_of(root, &id)?
            .with_context(|| format!("Reserved background task {id} has no launch record"))?;
        ensure!(
            stored.arguments == launch.arguments,
            "Reserved background task {id} has different launch arguments; refusing to reuse it"
        );
        ensure!(
            same_workspace(&stored.cwd, &launch.cwd),
            "Reserved background task {id} has a different canonical workspace; refusing to reuse it"
        );
        let progressed = job.launched
            || claim_token.is_some()
            || fact_present
            || job.supervisor_pid.is_some()
            || job.child_pid.is_some()
            || matches!(job.status.as_str(), "running" | "completed" | "cancelled");
        if progressed {
            // A supervisor was authorized (or a process recorded). Adopt the
            // durable state and never publish lifecycle state from here.
            let mut job = job;
            job.launched = true;
            return Ok(observed(job));
        }
        // No external authorization was published. A previous attempt failed
        // before it started a process; continue the same reserved identity.
        // This is the only point where the host writes lifecycle state after a
        // reservation, and it is still before any spawn.
        job.status = "starting".into();
        job.error = None;
        job.finished_ms = None;
        job.exit_code = None;
        job.supervisor_pid = None;
        job.child_pid = None;
        job.started_ms = now();
        job.heartbeat_ms = now();
        job.title = title.chars().take(160).collect();
        job.launched = false;
        // The instruction and workspace were validated above; only the host's
        // executable may have changed between attempts, so refresh it before
        // the worker reads `launch.json`.
        save(&dir.join("launch.json"), &launch)?;
        save(&dir.join("state.json"), &job)?;
        job
    } else {
        // A reserved job must be published before its name is returned, so a
        // crash after this point is always discoverable by exact identity.
        let job = Job {
            id: id.clone(),
            foreground,
            parent_session_id: parent.into(),
            session_id: session.into(),
            title: title.chars().take(160).collect(),
            status: "starting".into(),
            supervisor_pid: None,
            child_pid: None,
            started_ms: now(),
            heartbeat_ms: now(),
            finished_ms: None,
            exit_code: None,
            error: None,
            launched: false,
        };
        save(&dir.join("launch.json"), &launch)?;
        save(&dir.join("state.json"), &job)?;
        job
    };
    // A reserved job publishes its exclusive claim before the process starts.
    // This is the durable evidence that survives a crash between `spawn`
    // returning and the host recording it, and it is why a recovering host
    // adopts instead of starting a second supervisor.
    let token = reserved.map(|_| uuid::Uuid::new_v4().to_string());
    let mut created_claim = false;
    if let Some(token) = token.as_deref() {
        if !create_worker_claim(&dir, token)? {
            job.launched = true;
            return Ok(observed(job));
        }
        created_claim = true;
    }
    let mut claim = if foreground {
        Some(SessionClaim::acquire(root, session, &job.id)?)
    } else {
        None
    };
    let mut command = supervisor_command(supervisor, &dir, environment, token.as_deref());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000 | 0x00000200);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    match command.spawn() {
        Ok(mut child) => {
            if let Some(claim) = claim.as_mut() {
                claim.committed = true;
            }
            let supervisor_pid = child.id();
            // Reap when this host remains alive; dropping this thread on host
            // exit does not terminate the independently owned supervisor process.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            // Test-only boundary: production compiles this to a no-op.
            after_spawn_test_hook(&dir)?;
            // Publish the immutable spawn fact. The host never writes
            // `state.json` after a spawn: the supervisor is the single writer of
            // lifecycle state, so a fast worker's running/terminal publication
            // cannot be overwritten with a stale `starting` snapshot.
            write_spawn_fact(
                &dir,
                &SpawnFact {
                    supervisor_pid,
                    token: token.clone().unwrap_or_default(),
                    published_ms: now(),
                },
            )?;
            job.launched = true;
            job.supervisor_pid = Some(supervisor_pid);
            job.heartbeat_ms = now();
            Ok(job)
        }
        Err(error) => {
            // The supervisor never started. Revoke only the claim this attempt
            // created, then record a retryable failure below the spawn lock.
            if created_claim {
                if worker_claim_token(&dir)?.as_deref() == token.as_deref() {
                    let _ = fs::remove_file(worker_claim_path(&dir));
                }
            }
            job.status = "failed".into();
            job.error = Some(error.to_string());
            job.finished_ms = Some(now());
            save(&dir.join("state.json"), &job)?;
            Err(error).context("Starting background supervisor")
        }
    }
}

/// Returns true only in the dedicated supervisor process.
pub fn run_worker_from_env() -> Result<bool> {
    let Some(dir) = std::env::var_os(WORKER_ENV) else {
        return Ok(false);
    };
    std::env::remove_var(WORKER_ENV);
    run_worker(Path::new(&dir))?;
    Ok(true)
}

/// A supervisor may execute a job only when it can prove it owns the exclusive
/// claim. A reserved job publishes the claim (with a random token) before the
/// host starts any process and passes the same token to that supervisor only;
/// a duplicate supervisor has no matching token and exits. Non-reserved
/// supervisors keep the previous behavior and publish their own claim.
fn claim_worker(dir: &Path) -> Result<bool> {
    let env_token = std::env::var(WORKER_TOKEN_ENV)
        .ok()
        .filter(|token| !token.is_empty());
    match worker_claim_token(dir)? {
        Some(stored) => Ok(env_token.as_deref() == Some(stored.as_str())),
        None => create_worker_claim(dir, env_token.as_deref().unwrap_or_default()),
    }
}

fn run_worker(dir: &Path) -> Result<()> {
    // Exactly one supervisor may execute this job's CLI. A second supervisor
    // (for example one started by a host that recovered a reserved job while
    // the first was still starting) exits without touching durable state.
    if !claim_worker(dir)? {
        return Ok(());
    }
    let mut job = read_job(dir)?;
    let outcome = supervise(dir, &mut job);
    if let Err(error) = outcome {
        job.status = "failed".into();
        job.error = Some(format!("{error:#}"));
    }
    job.heartbeat_ms = now();
    job.finished_ms = Some(now());
    if job.foreground {
        let root = dir
            .parent()
            .and_then(Path::parent)
            .context("Invalid job root")?;
        SessionClaim::release(root, &job.session_id, &job.id)?;
    }
    save(&dir.join("state.json"), &job)
}

fn supervise(dir: &Path, job: &mut Job) -> Result<()> {
    let launch: Launch = serde_json::from_slice(&fs::read(dir.join("launch.json"))?)?;
    job.supervisor_pid = Some(std::process::id());
    if dir.join("cancel").exists() {
        job.status = "cancelled".into();
        return Ok(());
    }
    let mut command = Command::new(&launch.executable);
    command
        .args(&launch.arguments)
        .current_dir(&launch.cwd)
        .env_remove(WORKER_ENV)
        .env_remove(WORKER_TOKEN_ENV)
        .env(CONTROL_ENV, dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(fs::File::create(dir.join("output.jsonl"))?))
        .stderr(Stdio::from(fs::File::create(dir.join("stderr.log"))?));
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000 | 0x00000200);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().context("Starting background CLI task")?;
    job.child_pid = Some(child.id());
    job.status = "running".into();
    let mut cancel_started: Option<std::time::Instant> = None;
    loop {
        if let Some(status) = child.try_wait()? {
            job.exit_code = status.code();
            job.status = if dir.join("cancel").exists() {
                "cancelled"
            } else if status.success() {
                "completed"
            } else {
                "failed"
            }
            .into();
            if job.status == "failed" {
                // The worker records its own sanitized failure. Fall back to the
                // exit status only; stdout/stderr may contain user content.
                job.error = Some(
                    read_worker_failure(dir).unwrap_or_else(|| match job.exit_code {
                        Some(code) => format!("Worker exited with code {code}"),
                        None => "Worker exited without an exit code".to_owned(),
                    }),
                );
            }
            break;
        }
        if dir.join("cancel").exists()
            && cancel_started
                .get_or_insert_with(std::time::Instant::now)
                .elapsed()
                >= Duration::from_secs(2)
        {
            #[cfg(windows)]
            {
                use std::os::windows::process::CommandExt;
                // The supervisor still owns this unreaped child; no saved/stale PID is killed.
                let _ = Command::new("taskkill.exe")
                    .args(["/PID", &child.id().to_string(), "/T", "/F"])
                    .creation_flags(0x08000000)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
            #[cfg(unix)]
            {
                let _ = Command::new("kill")
                    .args(["-TERM", "--", &format!("-{}", child.id())])
                    .status();
            }
            let _ = child.kill();
            job.exit_code = child.wait()?.code();
            job.status = "cancelled".into();
            break;
        }
        job.heartbeat_ms = now();
        if let Err(error) = save(&dir.join("state.json"), job) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error).context("Saving supervisor heartbeat");
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    Ok(())
}

pub fn list(root: &Path, parent: Option<&str>) -> Result<Vec<Job>> {
    let directory = root.join("background-tasks");
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let mut jobs = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let job = match read_job(&entry.path()) {
            Ok(job) => job,
            // A new supervisor creates its directory before publishing state.
            // Another session's discovery must not fail during this window.
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                continue
            }
            // One damaged record must not prevent unrelated sessions from starting.
            // Preserve the original bytes and ownership claims for diagnosis; direct
            // reads and control operations still fail instead of guessing task state.
            Err(error) if error.downcast_ref::<serde_json::Error>().is_some() => {
                tracing::warn!(path = %entry.path().display(), error = %error,
                    "Skipping unreadable background task state; original file preserved");
                continue;
            }
            Err(error) => return Err(error),
        };
        if parent.map_or(true, |id| job.parent_session_id == id) {
            jobs.push(published(&entry.path(), job)?);
        }
    }
    jobs.sort_by_key(|job| job.started_ms);
    Ok(jobs)
}
pub fn get(root: &Path, id: &str) -> Result<Job> {
    let dir = directory(root, id)?;
    published(&dir, read_job(&dir)?)
}
pub fn cancel(root: &Path, id: &str) -> Result<Job> {
    let dir = directory(root, id)?;
    let job = observed(read_job(&dir)?);
    ensure!(
        matches!(job.status.as_str(), "starting" | "running"),
        "Task is {}; cannot cancel a stale or finished process",
        job.status
    );
    fs::write(dir.join("cancel"), b"cancel")?;
    Ok(job)
}
/// File a worker writes with its own sanitized failure, read by `/tasks` and
/// by hosts that only inspect task state.
pub const FAILURE_FILE: &str = "failure.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkerFailure {
    /// One-line, already sanitized diagnostic.
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempts: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retries: Option<u32>,
}

impl WorkerFailure {
    /// Build from the typed failure when one survived the turn.
    pub fn from_error(error: &anyhow::Error) -> Self {
        match crate::performance::find_failure(error) {
            Some(failure) => Self {
                summary: failure.diagnostic(),
                stage: Some(failure.stage().as_str().to_owned()),
                class: Some(failure.class().as_str().to_owned()),
                http: failure.status(),
                attempts: failure.attempts(),
                retries: failure.retries(),
            },
            None => Self {
                // Already sanitized: credentials, URLs and nested causes.
                summary: crate::performance::sanitized_chain(error),
                stage: None,
                class: None,
                http: None,
                attempts: None,
                retries: None,
            },
        }
    }
}

/// A worker in a background task records the failure for its supervisor.
/// Only this task's own directory is written; nothing is copied from output.
pub fn record_worker_failure(directory: &Path, error: &anyhow::Error) -> Result<()> {
    let failure = WorkerFailure::from_error(error);
    save(&directory.join(FAILURE_FILE), &failure)
}

fn read_worker_failure(dir: &Path) -> Option<String> {
    let bytes = std::fs::read(dir.join(FAILURE_FILE)).ok()?;
    let failure: WorkerFailure = serde_json::from_slice(&bytes).ok()?;
    (!failure.summary.trim().is_empty()).then_some(failure.summary)
}

pub fn output(root: &Path, id: &str) -> Result<(String, String)> {
    let dir = directory(root, id)?;
    read_job(&dir)?;
    let read = |name: &str| -> Result<String> {
        let path = dir.join(name);
        if !path.exists() {
            return Ok(String::new());
        }
        let mut file = fs::File::open(path)?;
        let size = file.metadata()?.len();
        // Read a bounded tail even when a tool produces very large output.
        use std::io::{Seek, SeekFrom};
        file.seek(SeekFrom::Start(size.saturating_sub(256 * 1024)))?;
        let mut bytes = Vec::new();
        file.take(256 * 1024).read_to_end(&mut bytes)?;
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    };
    Ok((read("output.jsonl")?, read("stderr.log")?))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingQuestion {
    pub request_id: String,
    pub question: String,
    pub kind: String,
    /// `{tool_name, input, tool_use_id}` for tool approvals, so an attached
    /// frontend can relay them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<serde_json::Value>,
}

pub fn pending_question(root: &Path, id: &str, parent: &str) -> Result<Option<PendingQuestion>> {
    let job = get(root, id)?;
    ensure!(
        job.parent_session_id == parent,
        "Task belongs to another session"
    );
    if !matches!(job.status.as_str(), "starting" | "running") {
        return Ok(None);
    }
    let path = directory(root, id)?.join("pending-question.json");
    for attempt in 0..=5 {
        match fs::read(&path) {
            Ok(bytes) => return Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            // Windows can report access denied while a resolved question is being removed.
            Err(error)
                if cfg!(windows) && matches!(error.raw_os_error(), Some(5 | 32)) && attempt < 5 =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Reading pending question {}", path.display()));
            }
        }
    }
    unreachable!()
}

pub fn answer_question(
    root: &Path,
    id: &str,
    parent: &str,
    request: &str,
    answer: &str,
) -> Result<()> {
    let question =
        pending_question(root, id, parent)?.context("Task is not waiting for an answer")?;
    ensure!(
        question.request_id == request,
        "Request ID does not match the pending question"
    );
    ensure!(answer.len() <= 64 * 1024, "Answer exceeds 64 KiB");
    let dir = directory(root, id)?;
    let path = response_path(&dir, request)?;
    let mut response = tempfile::NamedTempFile::new_in(&dir)?;
    serde_json::to_writer(
        &mut response,
        &serde_json::json!({"request_id":request,"answer":answer}),
    )?;
    response.flush()?;
    response
        .persist_noclobber(&path)
        .map_err(|error| error.error)
        .context("This question already has an answer or the response could not be published")?;
    if !pending_question(root, id, parent)?.is_some_and(|question| question.request_id == request) {
        let _ = fs::remove_file(path);
        anyhow::bail!("Question was resolved before the answer was published");
    }
    Ok(())
}

fn response_path(dir: &Path, request: &str) -> Result<PathBuf> {
    ensure!(
        !request.is_empty()
            && request.len() <= 128
            && request
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "Invalid background request ID"
    );
    Ok(dir.join(format!("answer-{request}.json")))
}

/// Cooperative cancellation lets the engine persist partial output/checkpoints
/// before the supervisor's bounded process-tree termination fallback.
pub async fn wait_for_cancellation() {
    let Some(value) = std::env::var_os(CONTROL_ENV) else {
        return std::future::pending().await;
    };
    let path = PathBuf::from(value).join("cancel");
    loop {
        if path.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// A supervised engine waits in place; no model request or tool is replayed.
/// The frontend may disconnect while this mailbox remains pending.
pub fn worker_directory(root: &Path, session: &str) -> Result<Option<PathBuf>> {
    let Some(value) = std::env::var_os(CONTROL_ENV) else {
        return Ok(None);
    };
    let dir = PathBuf::from(value);
    let job = read_job(&dir)?;
    ensure!(
        job.session_id == session
            && dir.canonicalize()? == directory(root, &job.id)?.canonicalize()?,
        "Background control channel does not own this session"
    );
    Ok(Some(dir))
}

pub async fn worker_answer(
    root: &Path,
    session: &str,
    request: &str,
    kind: &str,
    question: &str,
) -> Result<Option<String>> {
    let Some(dir) = worker_directory(root, session)? else {
        return Ok(None);
    };
    wait_for_answer(&dir, request, kind, question, None)
        .await
        .map(Some)
}

/// Like [`worker_answer`] for an AskUserQuestion question; `input` carries its
/// options, descriptions and previews so an attached frontend can show a menu.
pub async fn worker_question(
    root: &Path,
    session: &str,
    request: &str,
    question: &str,
    input: serde_json::Value,
) -> Result<Option<String>> {
    let Some(dir) = worker_directory(root, session)? else {
        return Ok(None);
    };
    wait_for_answer(&dir, request, "question", question, Some(input))
        .await
        .map(Some)
}

/// Like [`worker_answer`] for a tool approval with structured details.
pub async fn worker_permission(
    root: &Path,
    session: &str,
    request: &str,
    question: &str,
    tool: serde_json::Value,
) -> Result<Option<String>> {
    let Some(dir) = worker_directory(root, session)? else {
        return Ok(None);
    };
    wait_for_answer(&dir, request, "permission", question, Some(tool))
        .await
        .map(Some)
}

/// Like [`worker_answer`] for a plan review; `plan` carries the plan and its
/// file so an attached frontend can show the review menu.
pub async fn worker_plan(
    root: &Path,
    session: &str,
    request: &str,
    question: &str,
    plan: serde_json::Value,
) -> Result<Option<String>> {
    let Some(dir) = worker_directory(root, session)? else {
        return Ok(None);
    };
    wait_for_answer(&dir, request, "plan", question, Some(plan))
        .await
        .map(Some)
}

struct PendingAnswerCleanup {
    dir: PathBuf,
    request: String,
}

#[cfg(test)]
mod answer_cleanup_tests {
    use super::*;

    #[tokio::test]
    async fn cancelled_worker_question_is_removed_without_removing_next_generation() {
        let dir = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let path = dir.path().to_path_buf();
        let request = id.clone();
        let task = tokio::spawn(async move {
            wait_for_answer(&path, &request, "permission", "Allow?", None).await
        });
        tokio::time::timeout(Duration::from_secs(3), async {
            while !dir.path().join("pending-question.json").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(!dir.path().join("pending-question.json").exists());
        let guard = PendingAnswerCleanup {
            dir: dir.path().into(),
            request: id.clone(),
        };
        save(
            &dir.path().join("pending-question.json"),
            &PendingQuestion {
                request_id: "next".into(),
                question: "Next?".into(),
                kind: "permission".into(),
                tool: None,
            },
        )
        .unwrap();
        fs::write(response_path(dir.path(), &id).unwrap(), b"late").unwrap();
        drop(guard);
        let pending: PendingQuestion =
            serde_json::from_slice(&fs::read(dir.path().join("pending-question.json")).unwrap())
                .unwrap();
        assert_eq!(pending.request_id, "next");
        assert!(!response_path(dir.path(), &id).unwrap().exists());
    }
}
impl Drop for PendingAnswerCleanup {
    fn drop(&mut self) {
        let pending = self.dir.join("pending-question.json");
        if fs::read(&pending)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .is_some_and(|value| value["request_id"] == self.request)
        {
            let _ = fs::remove_file(pending);
        }
        if let Ok(response) = response_path(&self.dir, &self.request) {
            let _ = fs::remove_file(response);
        }
    }
}

async fn wait_for_answer(
    dir: &Path,
    request: &str,
    kind: &str,
    question: &str,
    tool: Option<serde_json::Value>,
) -> Result<String> {
    let _cleanup = PendingAnswerCleanup {
        dir: dir.to_path_buf(),
        request: request.into(),
    };
    save(
        &dir.join("pending-question.json"),
        &PendingQuestion {
            request_id: request.into(),
            question: question.into(),
            kind: kind.into(),
            tool,
        },
    )?;
    loop {
        ensure!(
            !dir.join("cancel").exists(),
            "Background task cancelled while awaiting an answer"
        );
        match fs::read(response_path(dir, request)?) {
            Ok(bytes) => {
                let value: serde_json::Value = serde_json::from_slice(&bytes)?;
                ensure!(
                    value["request_id"] == request,
                    "Mismatched background answer request ID"
                );
                let answer = value["answer"]
                    .as_str()
                    .context("Invalid background answer")?
                    .to_owned();
                // The request-bound guard removes only this generation. An older
                // waiter must never remove a newly published question.
                let pending: PendingQuestion =
                    serde_json::from_slice(&fs::read(dir.join("pending-question.json"))?)?;
                ensure!(
                    pending.request_id == request,
                    "Background question was superseded"
                );
                return Ok(answer);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub fn detach(root: &Path, id: &str, parent: &str) -> Result<()> {
    let job = get(root, id)?;
    ensure!(
        job.parent_session_id == parent,
        "Task belongs to another session"
    );
    fs::write(directory(root, id)?.join("detached"), b"detached")?;
    Ok(())
}

/// Incremental read-only attachment. Cursor advances over complete JSONL records
/// only; incomplete UTF-8 and records remain for the next poll.
pub fn events_since(
    root: &Path,
    id: &str,
    parent: &str,
    cursor: u64,
) -> Result<(Job, u64, Vec<serde_json::Value>)> {
    use std::io::{Seek, SeekFrom};
    let job = get(root, id)?;
    ensure!(
        job.parent_session_id == parent,
        "Task belongs to another session"
    );
    let path = directory(root, id)?.join("output.jsonl");
    if !path.exists() {
        return Ok((job, cursor, vec![]));
    }
    let mut file =
        fs::File::open(&path).with_context(|| format!("Opening task output {}", path.display()))?;
    ensure!(
        cursor <= file.metadata()?.len(),
        "Task output was truncated; attach again from the start"
    );
    file.seek(SeekFrom::Start(cursor))?;
    let mut bytes = Vec::new();
    file.take(8 * 1024 * 1024).read_to_end(&mut bytes)?;
    let mut end = bytes
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map(|index| index + 1)
        .unwrap_or(0);
    if !matches!(job.status.as_str(), "starting" | "running") && bytes.len() < 8 * 1024 * 1024 {
        end = bytes.len();
    }
    ensure!(
        end > 0 || bytes.len() < 8 * 1024 * 1024,
        "Task output record exceeds the 8 MiB attach limit"
    );
    let mut events = Vec::new();
    for line in bytes[..end]
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        events.push(serde_json::from_slice(line).context("Invalid background stream JSON")?);
    }
    Ok((job, cursor + end as u64, events))
}

/// Append a clearly attributed completion report atomically and exactly once.
/// A deterministic message ID is also the receipt, so failed transactions never
/// suppress a future retry and concurrent frontends cannot duplicate reports.
/// Receipt for a terminal state already presented by its foreground owner.
/// This never changes transcript content or child-session notifications.
pub fn acknowledge_foreground(root: &Path, id: &str, parent: &str) -> Result<()> {
    let job = get(root, id)?;
    ensure!(
        job.session_id == parent && job.parent_session_id == parent,
        "Not this session's foreground task"
    );
    ensure!(
        matches!(job.status.as_str(), "completed" | "failed" | "cancelled"),
        "Worker has not finished"
    );
    fs::write(
        directory(root, id)?.join("foreground-observed"),
        b"observed",
    )?;
    Ok(())
}

pub fn collect_completed(
    store: &crate::runtime::SessionStore,
    root: &Path,
    parent: &str,
) -> Result<Vec<String>> {
    let mut notices = Vec::new();
    for job in list(root, Some(parent))? {
        if job.session_id == parent
            && (!directory(root, &job.id)?.join("detached").exists()
                || directory(root, &job.id)?
                    .join("foreground-observed")
                    .exists())
        {
            continue;
        }
        if !matches!(job.status.as_str(), "completed" | "failed" | "cancelled") {
            continue;
        }
        let message_id = format!("background-report:{}:{}", parent, job.id);
        let exists = store
            .db
            .with_conn(|conn| crate::db::message_repo::get_message(conn, &message_id))??
            .is_some();
        if exists {
            continue;
        }
        let execution_session = super::session_worker::job_execution_session(root, &job)?;
        let (output, errors) = output(root, &job.id)?;
        let result = output
            .lines()
            .rev()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find(|event| event["type"] == "result")
            .and_then(|event| event["result"].as_str().map(str::to_owned));
        let result = if execution_session != job.session_id {
            store
                .db
                .with_conn(|conn| {
                    crate::db::message_repo::get_messages_by_conversation(conn, &execution_session)
                })??
                .into_iter()
                .rev()
                .find(|message| message.role == "assistant" && !message.content.trim().is_empty())
                .map(|message| message.content)
                .or(result)
        } else if job.session_id == parent {
            Some(if job.status == "completed" {
                "The worker completed and stored its answer directly in this session. This notice does not duplicate that answer.".to_owned()
            } else {
                format!("The worker ended with status {}. Review this session for any persisted partial output and /tasks show {} for logs. {}", job.status, job.id, job.error.as_deref().unwrap_or(""))
            })
        } else {
            result
        };
        let body = result.filter(|text| !text.is_empty()).unwrap_or_else(|| {
            job.error.clone().unwrap_or_else(|| {
                if errors.is_empty() {
                    "No final answer is available in the bounded output log; inspect the source session for the full transcript.".into()
                } else {
                    errors.chars().take(8192).collect()
                }
            })
        });
        let excerpt: String = body.chars().take(16384).collect();
        let report = format!("[Background task report]\nTask: {}\nSource session: {}\nStatus: {}\nTitle: {}\n\n{}{}\n[End background task report]", job.id, execution_session, job.status, job.title, excerpt, if excerpt.len()<body.len() { "\n[Excerpt truncated; use /tasks show for details.]" } else { "" });
        let inserted = store.db.with_conn(|conn| -> Result<bool> {
            let tx = rusqlite::Transaction::new_unchecked(
                conn,
                rusqlite::TransactionBehavior::Immediate,
            )?;
            if crate::db::message_repo::get_message(&tx, &message_id)?.is_some() {
                return Ok(false);
            }
            let order: i64 = tx.query_row(
                "SELECT COALESCE(MAX(sort_order), -1) + 1 FROM messages WHERE conversation_id = ?1",
                [parent],
                |row| row.get(0),
            )?;
            let now = chrono::Utc::now().to_rfc3339();
            crate::db::message_repo::insert_message(
                &tx,
                &message_id,
                parent,
                "system",
                &report,
                None,
                &now,
                false,
                order,
            )?;
            crate::db::conversation_repo::increment_message_count(&tx, parent)?;
            crate::db::conversation_repo::update_conversation_timestamp(&tx, parent, &now)?;
            tx.commit()?;
            Ok(true)
        })??;
        if inserted {
            notices.push(report);
        }
    }
    Ok(notices)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Production supervisor entry for reserved-job tests. The wrapper fixture
    /// execs the test binary with `--exact` on this ignored test, so the tests
    /// exercise the real `run_worker_from_env`/`run_worker`/`supervise` path
    /// rather than a re-implementation of the protocol.
    #[test]
    #[ignore = "entry point for the reserved-job supervisor fixture"]
    fn run_worker_entry() {
        assert!(
            run_worker_from_env().unwrap(),
            "the fixture supervisor must run with WORKER_ENV set"
        );
    }

    struct RealWorker {
        _temp: tempfile::TempDir,
        root: PathBuf,
        supervisor: PathBuf,
        child: PathBuf,
        counter: PathBuf,
    }

    fn real_worker_fixture() -> RealWorker {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let counter = temp.path().join("child-runs.log");
        let test_binary = std::env::current_exe().unwrap();
        let supervisor = temp.path().join("supervisor.sh");
        fs::write(
            &supervisor,
            format!(
                "#!/bin/sh\nexec '{}' --exact 'services::background_sessions::tests::run_worker_entry' --ignored --nocapture\n",
                test_binary.display()
            ),
        )
        .unwrap();
        let child = temp.path().join("child.sh");
        fs::write(
            &child,
            format!("#!/bin/sh\necho run >> '{}'\nexit 0\n", counter.display()),
        )
        .unwrap();
        for script in [&supervisor, &child] {
            fs::set_permissions(script, fs::Permissions::from_mode(0o755)).unwrap();
        }
        RealWorker {
            root: temp.path().to_path_buf(),
            supervisor,
            child,
            counter,
            _temp: temp,
        }
    }

    fn child_runs(counter: &Path) -> usize {
        fs::read_to_string(counter)
            .map(|text| text.lines().filter(|line| !line.trim().is_empty()).count())
            .unwrap_or(0)
    }

    fn wait_for_terminal(dir: &Path) -> Result<()> {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            if let Ok(job) = read_job(dir) {
                if matches!(job.status.as_str(), "completed" | "failed" | "cancelled") {
                    return Ok(());
                }
            }
            if std::time::Instant::now() >= deadline {
                anyhow::bail!(
                    "the real supervisor did not publish a terminal state in {}",
                    dir.display()
                );
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_for_child_runs(counter: &Path, expected: usize) {
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        while child_runs(counter) < expected {
            assert!(
                std::time::Instant::now() < deadline,
                "the real supervisor did not run its child"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn reserved_launch(worker: &RealWorker) -> Launch {
        Launch {
            executable: worker.child.clone(),
            arguments: Vec::new(),
            cwd: worker.root.clone(),
        }
    }

    #[test]
    fn a_fast_worker_terminal_state_survives_parent_publication() {
        let worker = real_worker_fixture();
        let id = uuid::Uuid::new_v4().to_string();
        let dir = directory(&worker.root, &id).unwrap();
        install_after_spawn_test_hook(|dir| {
            let dir = dir.to_path_buf();
            // Deterministic barrier: the real supervisor has already published
            // its terminal state before the host continues its own publication.
            wait_for_terminal(&dir)
        });

        let job = spawn_reserved(
            &worker.root,
            &worker.supervisor,
            reserved_launch(&worker),
            "parent",
            "child",
            "fixture",
            &[],
            &id,
        )
        .unwrap();
        assert!(job.launched);
        assert_eq!(child_runs(&worker.counter), 1);

        let stored = get(&worker.root, &id).unwrap();
        assert_eq!(
            stored.status, "completed",
            "the host must not overwrite the supervisor's terminal state"
        );
        assert_eq!(stored.exit_code, Some(0));
        assert!(stored.finished_ms.is_some());
        assert_eq!(stored.parent_session_id, "parent");
        assert_eq!(stored.session_id, "child");
        assert!(stored.launched);
    }

    #[test]
    fn a_host_interruption_after_spawn_is_adopted_without_a_second_child() {
        let worker = real_worker_fixture();
        let id = uuid::Uuid::new_v4().to_string();
        let dir = directory(&worker.root, &id).unwrap();
        install_after_spawn_test_hook(|_dir| {
            // Injected host interruption: the supervisor was started, but the
            // host dies before recording the spawn fact or its launch handle.
            anyhow::bail!("simulated host interruption after spawn")
        });
        let error = spawn_reserved(
            &worker.root,
            &worker.supervisor,
            reserved_launch(&worker),
            "parent",
            "child",
            "fixture",
            &[],
            &id,
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("simulated host interruption after spawn"),
            "{error:#}"
        );
        assert!(dir.join("worker.claim").exists());
        assert!(!dir.join("spawn.json").exists());
        wait_for_child_runs(&worker.counter, 1);
        wait_for_terminal(&dir).unwrap();

        // Recovery must adopt the authorized job, not start a second one.
        let recovered = spawn_reserved(
            &worker.root,
            &worker.supervisor,
            reserved_launch(&worker),
            "parent",
            "child",
            "fixture",
            &[],
            &id,
        )
        .unwrap();
        assert_eq!(recovered.status, "completed");
        assert_eq!(child_runs(&worker.counter), 1);
    }

    #[test]
    fn reserved_adoption_validates_identity_before_progressed_state() {
        let worker = real_worker_fixture();
        let canonical_root = worker.root.canonicalize().unwrap();
        let other_root = canonical_root.join("other-workspace");
        fs::create_dir_all(&other_root).unwrap();
        for (status, launched, finished) in [
            ("starting", false, None),
            ("launched", true, None),
            ("running", true, None),
            ("completed", true, Some(1234)),
        ] {
            for mismatch in ["parent", "session", "arguments", "workspace"] {
                let id = uuid::Uuid::new_v4().to_string();
                let dir = directory(&worker.root, &id).unwrap();
                fs::create_dir_all(&dir).unwrap();
                let stored_launch = Launch {
                    executable: worker.child.clone(),
                    arguments: vec!["--original".into()],
                    cwd: canonical_root.clone(),
                };
                save(&dir.join("launch.json"), &stored_launch).unwrap();
                let stored = Job {
                    id: id.clone(),
                    foreground: false,
                    parent_session_id: "parent".into(),
                    session_id: "child".into(),
                    title: "fixture".into(),
                    status: status.into(),
                    supervisor_pid: launched.then_some(11),
                    child_pid: launched.then_some(12),
                    started_ms: 1,
                    heartbeat_ms: 1,
                    finished_ms: finished,
                    exit_code: finished.map(|_| 0),
                    error: None,
                    launched,
                };
                save(&dir.join("state.json"), &stored).unwrap();
                let before = fs::read(dir.join("state.json")).unwrap();

                let mut requested = stored_launch;
                if mismatch == "arguments" {
                    requested.arguments = vec!["--different".into()];
                }
                if mismatch == "workspace" {
                    requested.cwd = other_root.clone();
                }
                let error = spawn_reserved(
                    &worker.root,
                    &worker.supervisor,
                    requested,
                    if mismatch == "parent" { "another-parent" } else { "parent" },
                    if mismatch == "session" { "another-child" } else { "child" },
                    "fixture",
                    &[],
                    &id,
                )
                .unwrap_err();
                let message = format!("{error:#}");
                assert!(
                    message.contains("another Router session")
                        || message.contains("another child session")
                        || message.contains("different launch arguments")
                        || message.contains("different canonical workspace"),
                    "{status} {mismatch}: {message}"
                );
                assert_eq!(
                    fs::read(dir.join("state.json")).unwrap(),
                    before,
                    "{status} {mismatch}: a refused adoption must not rewrite state"
                );
                assert!(
                    !dir.join("worker.claim").exists(),
                    "{status} {mismatch}: a refused adoption must not publish a claim"
                );
                assert!(!dir.join(SPAWN_FACT_FILE).exists());
            }
        }
        assert_eq!(child_runs(&worker.counter), 0);
    }

    #[test]
    fn a_matching_progressed_job_is_adopted_without_spawning() {
        let worker = real_worker_fixture();
        let id = uuid::Uuid::new_v4().to_string();
        let dir = directory(&worker.root, &id).unwrap();
        fs::create_dir_all(&dir).unwrap();
        let launch = reserved_launch(&worker);
        save(&dir.join("launch.json"), &launch).unwrap();
        save(
            &dir.join("state.json"),
            &Job {
                id: id.clone(),
                foreground: false,
                parent_session_id: "parent".into(),
                session_id: "child".into(),
                title: "fixture".into(),
                status: "completed".into(),
                supervisor_pid: Some(11),
                child_pid: Some(12),
                started_ms: 1,
                heartbeat_ms: 1,
                finished_ms: Some(1234),
                exit_code: Some(0),
                error: None,
                launched: true,
            },
        )
        .unwrap();
        let adopted = spawn_reserved(
            &worker.root,
            Path::new("/nonexistent-supervisor-must-not-run"),
            launch,
            "parent",
            "child",
            "fixture",
            &[],
            &id,
        )
        .unwrap();
        assert_eq!(adopted.status, "completed");
        assert_eq!(adopted.finished_ms, Some(1234));
        assert!(!dir.join("worker.claim").exists());
        assert_eq!(child_runs(&worker.counter), 0);
    }

    #[test]
    fn a_duplicate_supervisor_without_the_claim_token_does_not_execute_the_job() {
        let worker = real_worker_fixture();
        let id = uuid::Uuid::new_v4().to_string();
        let dir = directory(&worker.root, &id).unwrap();
        spawn_reserved(
            &worker.root,
            &worker.supervisor,
            reserved_launch(&worker),
            "parent",
            "child",
            "fixture",
            &[],
            &id,
        )
        .unwrap();
        wait_for_terminal(&dir).unwrap();
        assert_eq!(child_runs(&worker.counter), 1);
        let before = fs::read(dir.join("state.json")).unwrap();

        for token in [None, Some("a-different-token")] {
            let mut command = Command::new(&worker.supervisor);
            command
                .env(WORKER_ENV, &dir)
                .env_remove(WORKER_TOKEN_ENV)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            if let Some(token) = token {
                command.env(WORKER_TOKEN_ENV, token);
            }
            let status = command.status().unwrap();
            assert!(status.success(), "a duplicate supervisor exits cleanly");
            assert_eq!(
                child_runs(&worker.counter),
                1,
                "the duplicate supervisor must not run the child ({token:?})"
            );
            assert_eq!(
                fs::read(dir.join("state.json")).unwrap(),
                before,
                "the duplicate supervisor must not corrupt state"
            );
        }
    }
    #[test]
    fn ordinary_children_never_receive_the_worker_control_variables() {
        let mut command = Command::new("fixture");
        command
            .env(CONTROL_ENV, "fixture-control")
            .env(WORKER_ENV, "fixture-job")
            .env("DCL_KEEP_ME", "kept");
        strip_internal_env(&mut command);
        let removed: Vec<_> = command.get_envs().collect();
        assert!(removed
            .iter()
            .any(|(name, value)| *name == CONTROL_ENV && value.is_none()));
        assert!(removed
            .iter()
            .any(|(name, value)| *name == WORKER_ENV && value.is_none()));
        // Ordinary user variables keep their value.
        assert!(removed
            .iter()
            .any(|(name, value)| *name == "DCL_KEEP_ME"
                && *value == Some(std::ffi::OsStr::new("kept"))));
    }
    #[test]
    fn managed_workers_drop_inherited_model_state_and_keep_explicit_values() {
        let dir = Path::new("fixture-job");
        let command = supervisor_command(
            Path::new("fixture"),
            dir,
            &[(WORKER_MODEL_ENV.to_owned(), "explicit-model".to_owned())],
            Some("reserved-token"),
        );
        let envs: Vec<_> = command.get_envs().collect();
        assert!(envs
            .iter()
            .any(|(name, value)| *name == WORKER_AUTO_ROUTE_ENV && value.is_none()));
        assert!(envs
            .iter()
            .any(|(name, value)| *name == CONTROL_ENV && value.is_none()));
        assert!(envs.iter().any(|(name, value)| *name == WORKER_ENV
            && *value == Some(dir.as_os_str())));
        // Delegation passes a deliberate snapshot; it is applied after the strip.
        assert!(envs.iter().any(|(name, value)| *name == WORKER_MODEL_ENV
            && *value == Some(std::ffi::OsStr::new("explicit-model"))));
    }

    fn fixture(script: &str) -> (tempfile::TempDir, PathBuf, String) {
        let root = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let dir = directory(root.path(), &id).unwrap();
        fs::create_dir_all(&dir).unwrap();
        let job = Job {
            id: id.clone(),
            foreground: false,
            parent_session_id: "parent".into(),
            session_id: "child".into(),
            title: "fixture".into(),
            status: "starting".into(),
            supervisor_pid: None,
            child_pid: None,
            started_ms: now(),
            heartbeat_ms: now(),
            finished_ms: None,
            exit_code: None,
            error: None,
            launched: false,
        };
        save(&dir.join("state.json"), &job).unwrap();
        save(
            &dir.join("launch.json"),
            &Launch {
                executable: "node".into(),
                arguments: vec!["-e".into(), script.into()],
                cwd: root.path().into(),
            },
        )
        .unwrap();
        (root, dir, id)
    }
    #[cfg(windows)]
    #[test]
    fn pending_question_survives_short_file_lock_but_reports_persistent_failure() {
        use std::os::windows::fs::OpenOptionsExt;
        let (root, dir, id) = fixture("");
        let path = dir.join("pending-question.json");
        save(
            &path,
            &PendingQuestion {
                request_id: "request-1".into(),
                question: "Allow test?".into(),
                kind: "permission".into(),
                tool: None,
            },
        )
        .unwrap();
        let lock = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            drop(lock);
        });
        let question = pending_question(root.path(), &id, "parent")
            .unwrap()
            .unwrap();
        release.join().unwrap();
        assert_eq!(question.request_id, "request-1");

        let lock = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .unwrap();
        let started = std::time::Instant::now();
        let error = pending_question(root.path(), &id, "parent").unwrap_err();
        assert!(error.to_string().contains("Reading pending question"));
        assert!(error.to_string().contains(path.to_str().unwrap()));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(pending_question(root.path(), &id, "foreign")
            .unwrap_err()
            .to_string()
            .contains("another session"));
        drop(lock);
        fs::remove_file(&path).unwrap();
        assert!(pending_question(root.path(), &id, "parent")
            .unwrap()
            .is_none());
        assert!(!dir.join("cancel").exists());
    }

    #[test]
    fn discovery_tolerates_concurrent_unpublished_jobs_and_bad_state() {
        let (root, dir, id) = fixture("");
        let new_id = uuid::Uuid::new_v4().to_string();
        let new_dir = directory(root.path(), &new_id).unwrap();
        let (created, ready) = std::sync::mpsc::channel();
        let (publish, allowed) = std::sync::mpsc::channel();
        let mut next = read_job(&dir).unwrap();
        next.id = new_id.clone();
        next.parent_session_id = "other-session".into();
        let writer = std::thread::spawn(move || {
            fs::create_dir_all(&new_dir).unwrap();
            created.send(()).unwrap();
            allowed.recv().unwrap();
            save(&new_dir.join("state.json"), &next).unwrap();
        });
        ready.recv().unwrap();
        for _ in 0..50 {
            let jobs = list(root.path(), Some("parent")).unwrap();
            assert_eq!(jobs.len(), 1);
            assert_eq!(jobs[0].id, id);
        }
        publish.send(()).unwrap();
        writer.join().unwrap();
        assert_eq!(list(root.path(), None).unwrap().len(), 2);
        fs::write(
            directory(root.path(), &new_id).unwrap().join("state.json"),
            b"invalid",
        )
        .unwrap();
        assert_eq!(list(root.path(), None).unwrap().len(), 1);
        assert!(
            format!("{:#}", get(root.path(), &new_id).unwrap_err()).contains("Parsing task state")
        );
    }

    #[test]
    fn discovery_preserves_crash_damaged_records_and_ownership() {
        let (root, dir, id) = fixture("");
        let claim = root.path().join("background-tasks/session-test.claim");
        fs::write(&claim, &id).unwrap();
        for bytes in [
            Vec::new(),
            vec![0; 409],
            b"{\"id\":".to_vec(),
            b"{}".to_vec(),
            vec![255],
        ] {
            fs::write(dir.join("state.json"), &bytes).unwrap();
            assert!(list(root.path(), None).unwrap().is_empty());
            assert!(list(root.path(), Some("parent")).unwrap().is_empty());
            assert!(get(root.path(), &id).is_err());
            assert!(cancel(root.path(), &id).is_err());
            assert_eq!(fs::read(dir.join("state.json")).unwrap(), bytes);
            assert_eq!(fs::read_to_string(&claim).unwrap(), id);
        }
        // Filesystem failures are not classified as damaged JSON.
        fs::remove_file(dir.join("state.json")).unwrap();
        fs::create_dir(dir.join("state.json")).unwrap();
        assert!(list(root.path(), None).is_err());
    }

    #[test]
    fn heartbeat_replacement_survives_concurrent_readers() {
        use std::sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        };
        let (_root, dir, _) = fixture("");
        let done = Arc::new(AtomicBool::new(false));
        let reader_dir = dir.clone();
        let reader_done = done.clone();
        let reader = std::thread::spawn(move || {
            while !reader_done.load(Ordering::Relaxed) {
                read_job(&reader_dir).unwrap();
            }
        });
        let mut job = read_job(&dir).unwrap();
        for index in 0..100 {
            job.heartbeat_ms = index;
            save(&dir.join("state.json"), &job).unwrap();
        }
        done.store(true, Ordering::Relaxed);
        reader.join().unwrap();
        assert_eq!(read_job(&dir).unwrap().heartbeat_ms, 99);
    }
    #[test]
    fn foreground_claim_is_exclusive_and_released_only_by_owner() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("background-tasks")).unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let mut claim = SessionClaim::acquire(root.path(), &session, "owner").unwrap();
        assert!(SessionClaim::acquire(root.path(), &session, "other").is_err());
        SessionClaim::release(root.path(), &session, "other").unwrap();
        assert!(claim.path.exists());
        claim.committed = true;
        drop(claim);
        assert!(SessionClaim::acquire(root.path(), &session, "other").is_err());
        SessionClaim::release(root.path(), &session, "owner").unwrap();
        let retry = SessionClaim::acquire(root.path(), &session, "retry").unwrap();
        let path = retry.path.clone();
        drop(retry);
        assert!(!path.exists());
    }
    #[tokio::test]
    async fn worker_mailbox_resumes_same_request_and_rejects_wrong_owner() {
        let (root, dir, id) = fixture("");
        let worker_dir = dir.clone();
        let worker = tokio::spawn(async move {
            wait_for_answer(&worker_dir, "request-1", "permission", "Allow test?", None).await
        });
        for _ in 0..50 {
            if pending_question(root.path(), &id, "parent")
                .unwrap()
                .is_some()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(answer_question(root.path(), &id, "foreign", "request-1", "yes").is_err());
        assert!(answer_question(root.path(), &id, "parent", "wrong-request", "yes").is_err());
        answer_question(root.path(), &id, "parent", "request-1", "no").unwrap();
        assert!(answer_question(root.path(), &id, "parent", "request-1", "yes").is_err());
        assert_eq!(worker.await.unwrap().unwrap(), "no");
        assert!(pending_question(root.path(), &id, "parent")
            .unwrap()
            .is_none());
        assert!(!response_path(&dir, "request-1").unwrap().exists());
    }
    #[tokio::test]
    async fn worker_mailbox_cancellation_does_not_approve() {
        let (_root, dir, _) = fixture("");
        fs::write(dir.join("cancel"), b"cancel").unwrap();
        assert!(
            wait_for_answer(&dir, "request", "permission", "Allow?", None)
                .await
                .is_err()
        );
    }
    #[test]
    fn attachment_preserves_partial_records_and_rejects_foreign_parent() {
        let (root, dir, id) = fixture("");
        fs::write(
            dir.join("output.jsonl"),
            b"{\"type\":\"text\",\"delta\":\"first\"}\n{\"type\":",
        )
        .unwrap();
        let (_, cursor, events) = events_since(root.path(), &id, "parent", 0).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["delta"], "first");
        let (_, next, events) = events_since(root.path(), &id, "parent", cursor).unwrap();
        assert_eq!(next, cursor);
        assert!(events.is_empty());
        let mut file = fs::OpenOptions::new()
            .append(true)
            .open(dir.join("output.jsonl"))
            .unwrap();
        file.write_all(b"\"result\",\"result\":\"done\"}\n")
            .unwrap();
        let (_, next, events) = events_since(root.path(), &id, "parent", cursor).unwrap();
        assert!(next > cursor);
        assert_eq!(events[0]["result"], "done");
        assert!(events_since(root.path(), &id, "foreign", 0).is_err());
    }
    #[test]
    fn completion_receipt_is_atomic_and_survives_reopen() {
        let (root, dir, _) = fixture("");
        let paths = crate::runtime::RuntimePaths::new(root.path().to_path_buf()).unwrap();
        let store = crate::runtime::SessionStore::new(paths.open_database().unwrap());
        let parent = store.create(root.path(), "model", "provider").unwrap();
        let mut job = read_job(&dir).unwrap();
        job.parent_session_id = parent.clone();
        job.status = "completed".into();
        job.finished_ms = Some(now());
        save(&dir.join("state.json"), &job).unwrap();
        fs::write(
            dir.join("output.jsonl"),
            b"{\"type\":\"result\",\"result\":\"child answer\"}\n",
        )
        .unwrap();
        let notices = collect_completed(&store, root.path(), &parent).unwrap();
        assert_eq!(notices.len(), 1);
        assert!(notices[0].contains("child answer"));
        assert!(collect_completed(&store, root.path(), &parent)
            .unwrap()
            .is_empty());
        drop(store);
        let store = crate::runtime::SessionStore::new(paths.open_database().unwrap());
        assert!(collect_completed(&store, root.path(), &parent)
            .unwrap()
            .is_empty());
        let history = store.history(&parent).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0]["role"], "system");
        assert!(history[0]["content"].as_str().unwrap().contains(&job.id));
    }
    #[test]
    fn foreground_failure_has_no_background_echo_but_detached_failure_reports_once() {
        let (root, dir, id) = fixture("");
        let paths = crate::runtime::RuntimePaths::new(root.path().to_path_buf()).unwrap();
        let store = crate::runtime::SessionStore::new(paths.open_database().unwrap());
        let parent = store.create(root.path(), "model", "provider").unwrap();
        store
            .append(&parent, "assistant", "Persisted partial answer")
            .unwrap();
        let mut job = read_job(&dir).unwrap();
        job.parent_session_id = parent.clone();
        job.session_id = parent.clone();
        job.foreground = true;
        job.status = "failed".into();
        job.finished_ms = Some(now());
        job.error = Some("original full diagnostic".into());
        save(&dir.join("state.json"), &job).unwrap();
        fs::write(
            dir.join("output.jsonl"),
            b"{\"type\":\"error\",\"error\":\"original full diagnostic\"}\n",
        )
        .unwrap();
        acknowledge_foreground(root.path(), &id, &parent).unwrap();
        detach(root.path(), &id, &parent).unwrap();
        assert!(collect_completed(&store, root.path(), &parent)
            .unwrap()
            .is_empty());
        assert_eq!(
            store.history(&parent).unwrap()[0]["content"],
            "Persisted partial answer"
        );
        assert!(output(root.path(), &id)
            .unwrap()
            .0
            .contains("original full diagnostic"));
        fs::remove_file(dir.join("foreground-observed")).unwrap();
        assert_eq!(
            collect_completed(&store, root.path(), &parent)
                .unwrap()
                .len(),
            1
        );
        assert!(collect_completed(&store, root.path(), &parent)
            .unwrap()
            .is_empty());
        assert_eq!(
            store.history(&parent).unwrap()[0]["content"],
            "Persisted partial answer"
        );
    }

    #[test]
    fn records_real_exit_and_output() {
        let (root, dir, id) = fixture("process.stdout.write('hello 中文');process.exit(0)");
        run_worker(&dir).unwrap();
        let job = get(root.path(), &id).unwrap();
        assert_eq!(job.status, "completed");
        assert_eq!(job.exit_code, Some(0));
        assert!(output(root.path(), &id).unwrap().0.contains("中文"));
        assert_eq!(list(root.path(), Some("other")).unwrap().len(), 0);
    }
    #[test]
    fn cancellation_terminates_owned_process_and_stale_jobs_are_not_running() {
        let (root, dir, id) = fixture("setInterval(()=>{},1000)");
        let worker_dir = dir.clone();
        let worker = std::thread::spawn(move || run_worker(&worker_dir).unwrap());
        for _ in 0..80 {
            if get(root.path(), &id).unwrap().status == "running" {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        cancel(root.path(), &id).unwrap();
        worker.join().unwrap();
        assert_eq!(get(root.path(), &id).unwrap().status, "cancelled");
        assert!(cancel(root.path(), &id).is_err());
        let mut job = read_job(&dir).unwrap();
        job.status = "running".into();
        job.heartbeat_ms = 0;
        save(&dir.join("state.json"), &job).unwrap();
        assert_eq!(get(root.path(), &id).unwrap().status, "unresponsive");
        assert!(cancel(root.path(), &id).is_err());
        assert!(get(root.path(), "../escape").is_err());
    }
    #[test]
    fn reports_nonzero_exit_and_launch_failure() {
        let (root, dir, id) = fixture("process.exit(7)");
        run_worker(&dir).unwrap();
        let job = get(root.path(), &id).unwrap();
        assert_eq!(job.status, "failed");
        assert_eq!(job.exit_code, Some(7));

        // A job ID is one-shot: the worker claim prevents re-executing the same
        // job after a completed run, so the launch-failure case uses a new job.
        let (root, dir, id) = fixture("");
        save(
            &dir.join("launch.json"),
            &Launch {
                executable: "definitely-no-such-cli-executable".into(),
                arguments: vec![],
                cwd: root.path().into(),
            },
        )
        .unwrap();
        run_worker(&dir).unwrap();
        assert!(get(root.path(), &id).unwrap().error.is_some());
    }

    #[test]
    fn a_second_supervisor_does_not_execute_the_same_job() {
        let (root, dir, id) = fixture("process.stdout.write('once')");
        run_worker(&dir).unwrap();
        let first_output = output(root.path(), &id).unwrap().0;
        assert_eq!(first_output.matches("once").count(), 1);
        // The claim is durable, so a repeated supervisor run is a no-op.
        fs::write(dir.join("output.jsonl"), b"").unwrap();
        run_worker(&dir).unwrap();
        assert_eq!(output(root.path(), &id).unwrap().0, "");
        assert!(dir.join("worker.claim").exists());
    }
}
