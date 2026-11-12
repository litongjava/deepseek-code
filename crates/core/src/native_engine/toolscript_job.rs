//! Durable ToolScript jobs.
//!
//! A script can outlive the turn that started it, and `dcl --print` exits as
//! soon as its turn ends. A detached `tokio::spawn` would therefore lose the
//! script silently. Every run is instead owned by a dedicated supervisor
//! process re-executed from the host binary (the same pattern as
//! `services::background_sessions`), and the owner holds an OS advisory lease
//! for its whole lifetime. Liveness is decided by that lease, never by a PID or
//! heartbeat.
//!
//! The task record, `output.log` and `result.json` are durable independently of
//! the parent; execution is owned by the worker. A terminal state is written
//! exactly once and is never turned back into `running`.

use super::engine_core::ReasoningMode;
use super::tool_loop::{EngineEvent, ToolLoopExecutor};
use crate::native_engine::provider_manager::ProviderManager;
use crate::services::tool_history::{self, ToolOrigins};
use crate::toolscript::Cancellation;
use anyhow::{anyhow, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;

/// Set on the supervisor process: the absolute job directory it owns.
pub const WORKER_ENV: &str = "DEEPSEEK_CODE_TOOLSCRIPT_JOB";
/// Test/embedding override for the supervisor executable.
pub const WORKER_EXE_ENV: &str = "DEEPSEEK_CODE_TOOLSCRIPT_WORKER_EXE";
/// Optional runtime credential channel; never written into a job file.
const CHILD_KEY_ENV: &str = "DEEPSEEK_CODE_CHILD_API_KEY";

/// How long admission may take before the start is reported as failed.
const ADMISSION_TIMEOUT: Duration = Duration::from_secs(20);
/// Bounded response wait for a foreground script; it never cancels the script.
pub const FOREGROUND_WAIT: Duration = Duration::from_secs(60);
/// Bounded read for a non-paged snapshot.
const SNAPSHOT_LIMIT: u64 = 1024 * 1024;
const RESULT_LIMIT: u64 = 2 * 1024 * 1024;
/// How long an approval request may wait for an answer before it is denied.
/// A parent that exited (for example `--print`) leaves no responder, so the
/// call fails closed instead of hanging.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);
/// How long cancellation waits for the owned process groups to be gone.
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(15);
/// How often the owner re-reads the durable permission policy.
const POLICY_REFRESH: Duration = Duration::from_millis(500);
/// A freshly spawned owner has not taken the lease yet. Recovery must not call
/// that `starting` window an owner loss; the admission handshake allows
/// `ADMISSION_TIMEOUT`, and this grace is strictly larger.
const STARTING_GRACE_MS: u64 = 25_000;

const RUNNING: &str = "running";
const TERMINAL: &[&str] = &["completed", "failed", "cancelled", "interrupted"];

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn is_terminal(status: &str) -> bool {
    TERMINAL.contains(&status)
}

pub fn jobs_dir(root: &Path) -> PathBuf {
    root.join("toolscript-jobs")
}

pub fn directory(root: &Path, id: &str) -> Result<PathBuf> {
    // The id is a task UUID; reject anything that could escape the root.
    let id = uuid::Uuid::parse_str(id)
        .context("Invalid ToolScript job id")?
        .to_string();
    Ok(jobs_dir(root).join(id))
}

fn save_atomic<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().context("Missing job directory")?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut temporary, value)?;
    use std::io::Write as _;
    temporary.flush()?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

/// Everything the owner needs to run one script. Deliberately contains no
/// credential: the provider is re-resolved from configuration by the worker.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSpec {
    pub id: String,
    pub session: String,
    pub workspace: String,
    pub assistant_id: Option<String>,
    pub parent_tool_use_id: String,
    pub provider_id: String,
    pub model: String,
    pub source: String,
    pub args: Value,
    pub data_dir: String,
    pub settings_path: String,
    pub loaded_tools: Vec<String>,
    pub reasoning: ReasoningMode,
    pub permission_mode: String,
    pub allowed_tools: Option<Vec<String>>,
    pub exposed_tools: Option<Vec<String>>,
    pub source_sha256: String,
    /// Whether a live parent can answer permission prompts. A background job
    /// and a `--print` parent are non-interactive: a call that needs approval
    /// is denied explicitly rather than auto-allowed or left hanging.
    #[serde(default)]
    pub interactive: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobState {
    pub id: String,
    pub kind: String,
    pub session: String,
    pub parent_tool_use_id: String,
    pub assistant_id: Option<String>,
    pub status: String,
    pub owner_pid: Option<u32>,
    pub started_ms: u64,
    pub finished_ms: Option<u64>,
    pub termination: Option<String>,
    pub error: Option<String>,
    pub calls_started: u64,
    pub calls_succeeded: u64,
}

impl JobState {
    fn new(spec: &JobSpec) -> Self {
        Self {
            id: spec.id.clone(),
            kind: "toolscript".into(),
            session: spec.session.clone(),
            parent_tool_use_id: spec.parent_tool_use_id.clone(),
            assistant_id: spec.assistant_id.clone(),
            status: "starting".into(),
            owner_pid: None,
            started_ms: now_ms(),
            finished_ms: None,
            termination: None,
            error: None,
            calls_started: 0,
            calls_succeeded: 0,
        }
    }

    pub fn is_terminal(&self) -> bool {
        is_terminal(&self.status)
    }
}

fn load_spec(directory: &Path) -> Result<JobSpec> {
    Ok(serde_json::from_slice(&fs::read(
        directory.join("spec.json"),
    )?)?)
}

fn load_state(directory: &Path) -> Result<JobState> {
    Ok(serde_json::from_slice(&fs::read(
        directory.join("state.json"),
    )?)?)
}

/// Current state of one job, for a foreground caller polling its bounded wait.
pub fn state(directory: &Path) -> Result<JobState> {
    load_state(directory)
}

/// The tool-visible outcome of a terminal job.
pub fn terminal_output(directory: &Path, id: &str) -> Value {
    let state = load_state(directory).unwrap_or(JobState {
        id: id.into(),
        kind: "toolscript".into(),
        session: String::new(),
        parent_tool_use_id: String::new(),
        assistant_id: None,
        status: "failed".into(),
        owner_pid: None,
        started_ms: 0,
        finished_ms: None,
        termination: None,
        error: Some("The ToolScript job state could not be read.".into()),
        calls_started: 0,
        calls_succeeded: 0,
    });
    json!({
        "task_id": id,
        "status": state.status,
        "termination": state.termination,
        "error": state.error,
        "result": result_value(directory),
        "calls_started": state.calls_started,
        "calls_succeeded": state.calls_succeeded,
    })
}

/// A live owner holds this lock for its whole lifetime. Reading the lease is
/// the only liveness test: a PID can be reused and a heartbeat can be stale.
fn owner_is_live(directory: &Path) -> bool {
    let path = directory.join("lease");
    let Ok(file) = File::options()
        .read(true)
        .write(true)
        .create(true)
        .open(&path)
    else {
        // An unreadable lease is not proof of an owner; treat it as gone so a
        // stuck job is reported as interrupted rather than silently running.
        return false;
    };
    match crate::fs::try_lock_file(&file) {
        Ok(()) => {
            let _ = crate::fs::unlock_file(&file);
            false
        }
        Err(std::fs::TryLockError::WouldBlock) => true,
        Err(std::fs::TryLockError::Error(_)) => false,
    }
}

/// Holds the same exclusive lease a live owner uses, then re-reads and rewrites
/// the state. A new owner cannot start or finish in between, so the decision
/// cannot be made from a stale observation.
struct OwnedLease {
    file: File,
}

impl OwnedLease {
    /// `Ok(None)` means a live owner already holds the lease.
    fn acquire(directory: &Path) -> Result<Option<Self>> {
        let path = directory.join("lease");
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)?;
        match crate::fs::try_lock_file(&file) {
            Ok(()) => Ok(Some(Self { file })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
        }
    }
}

impl Drop for OwnedLease {
    fn drop(&mut self) {
        let _ = crate::fs::unlock_file(&self.file);
    }
}

/// Marks a non-terminal job whose owner is gone as `interrupted`.
///
/// Recovery and a new owner contend for the same lease: whoever holds it
/// decides, so a job cannot be marked interrupted while an owner is running,
/// and a recovered job cannot be taken over without re-reading its state.
pub fn recover_job(directory: &Path) -> Result<Option<JobState>> {
    let Some(_lease) = OwnedLease::acquire(directory)? else {
        return Ok(None);
    };
    let mut state = load_state(directory)?;
    if state.is_terminal() {
        return Ok(None);
    }
    // A job that was just handed to a new owner is still `starting` until that
    // owner takes the lease. Treating that window as an owner loss would race
    // a perfectly healthy start.
    if state.status == "starting" && now_ms().saturating_sub(state.started_ms) < STARTING_GRACE_MS {
        return Ok(None);
    }
    state.status = "interrupted".into();
    state.termination = Some("owner_lost".into());
    state.finished_ms = Some(now_ms());
    state.error = Some(
        "The ToolScript owner is gone; the script was not replayed and no result was produced."
            .into(),
    );
    save_atomic(&directory.join("state.json"), &state)?;
    Ok(Some(state))
}

/// Recovers every non-terminal job of one session. Called lazily by readers, so
/// a reopened session reports `interrupted` instead of a phantom `running`.
pub fn recover_session(root: &Path, session: &str) -> Result<()> {
    let dir = jobs_dir(root);
    let Ok(entries) = fs::read_dir(&dir) else {
        return Ok(());
    };
    for entry in entries.flatten() {
        if !entry.path().join("state.json").is_file() {
            continue;
        }
        if let Ok(state) = load_state(&entry.path()) {
            if state.session == session && !state.is_terminal() {
                let _ = recover_job(&entry.path());
            }
        }
    }
    Ok(())
}

pub struct StartOutcome {
    pub id: String,
    pub directory: PathBuf,
}

/// Admits a job and starts its owner. Returns only after the owner has taken
/// the lease and published `running`; any failure kills the exact child and
/// marks the record failed, so no phantom running task is left behind.
pub async fn start_job(root: &Path, mut spec: JobSpec) -> Result<StartOutcome> {
    let record = crate::tools::tasks::execute_for_session_async(
        "TaskCreate",
        json!({
            "subject": "ToolScript",
            "description": "A ToolScript run",
            "metadata": {"toolscript_job": true},
        }),
        root,
        &spec.session,
    )
    .await?;
    let id = record["id"]
        .as_str()
        .context("ToolScript task record has no id")?
        .to_owned();
    spec.id = id.clone();
    let directory = directory(root, &id)?;
    // A failure to record the job must not leave a pending record behind.
    let setup = (|| -> Result<()> {
        fs::create_dir_all(&directory)?;
        save_atomic(&directory.join("spec.json"), &spec)?;
        save_atomic(&directory.join("state.json"), &JobState::new(&spec))?;
        Ok(())
    })();
    if let Err(error) = setup {
        let message = format!("Could not record the ToolScript job: {error:#}");
        // There is no state record to mark here: writing it is what failed.
        // The task record created above must still not stay `pending`.
        return Err(
            record_start_failure(root, &spec.session, &id, &directory, &message, false).await,
        );
    }

    let executable = match std::env::var_os(WORKER_EXE_ENV)
        .map(PathBuf::from)
        .or_else(|| std::env::current_exe().ok())
    {
        Some(executable) => executable,
        None => {
            let message = "Cannot locate the host executable for the ToolScript owner".to_owned();
            return Err(
                record_start_failure(root, &spec.session, &id, &directory, &message, true).await,
            );
        }
    };
    let mut command = std::process::Command::new(executable);
    crate::services::background_sessions::strip_internal_env(&mut command);
    command
        .env(WORKER_ENV, &directory)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000 | 0x00000200);
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let message = format!("Starting the ToolScript owner: {error}");
            return Err(
                record_start_failure(root, &spec.session, &id, &directory, &message, true).await,
            );
        }
    };

    let deadline = Instant::now() + ADMISSION_TIMEOUT;
    loop {
        if let Ok(current) = load_state(&directory) {
            match classify_admission(&current) {
                Admission::Running => break,
                // A fast script can finish (or fail) before admission is
                // observed. Its real outcome is not a start failure.
                Admission::Terminal => {
                    reap(child);
                    return Ok(StartOutcome { id, directory });
                }
                Admission::Failed => {
                    let error = current
                        .error
                        .clone()
                        .unwrap_or_else(|| "The ToolScript owner failed during admission".into());
                    reap(child);
                    // The `failed`/`startup_failed` terminal state is already
                    // durable; only the task record needs the same outcome.
                    return Err(record_start_failure(
                        root,
                        &spec.session,
                        &id,
                        &directory,
                        &error,
                        false,
                    )
                    .await);
                }
                Admission::Pending => {}
            }
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                // The owner may have written a terminal state just before
                // exiting; a recorded outcome wins over the exit status.
                if let Ok(current) = load_state(&directory) {
                    if classify_admission(&current) == Admission::Terminal {
                        return Ok(StartOutcome { id, directory });
                    }
                }
                let error = format!("The ToolScript owner exited during admission ({status})");
                return Err(record_start_failure(
                    root,
                    &spec.session,
                    &id,
                    &directory,
                    &error,
                    true,
                )
                .await);
            }
            Ok(None) => {}
            Err(error) => {
                let message = format!("Cannot observe the ToolScript owner: {error}");
                kill_exact(&mut child);
                return Err(record_start_failure(
                    root,
                    &spec.session,
                    &id,
                    &directory,
                    &message,
                    true,
                )
                .await);
            }
        }
        if Instant::now() >= deadline {
            let message = "The ToolScript owner did not admit the job in time".to_owned();
            kill_exact(&mut child);
            return Err(
                record_start_failure(root, &spec.session, &id, &directory, &message, true).await,
            );
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // The owner is independent; reap it when this host stays alive so it does
    // not become a zombie. Dropping the thread on host exit does not stop it.
    reap(child);
    // The state update belongs to the lease-holding owner, which writes it
    // before it starts the script. The parent only observes, so it can never
    // move a record that already finished back to `running`.
    Ok(StartOutcome { id, directory })
}

/// What the admission handshake observed.
#[derive(Debug, PartialEq, Eq)]
enum Admission {
    /// The owner took the lease and is running.
    Running,
    /// The script already produced a real terminal outcome.
    Terminal,
    /// The owner failed to start; this is not a script outcome.
    Failed,
    /// Still starting or running; keep waiting.
    Pending,
}

fn classify_admission(state: &JobState) -> Admission {
    if state.status == RUNNING {
        return Admission::Running;
    }
    if state.is_terminal() {
        let startup_failed = state.termination.as_deref() == Some("startup_failed");
        if startup_failed || state.status == "interrupted" {
            return Admission::Failed;
        }
        return Admission::Terminal;
    }
    Admission::Pending
}

/// Reaps a spawned owner without blocking this host's exit. Every successful
/// handshake path takes this, so no child is left as a zombie.
fn reap(child: std::process::Child) {
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
    });
}

fn kill_exact(child: &mut std::process::Child) {
    let pid = child.id();
    #[cfg(unix)]
    {
        // The owner runs in its own process group; signal only that group.
        let _ = std::process::Command::new("kill")
            .args(["-9", "--", &format!("-{pid}")])
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// What `mark_failed` did with the durable state. Callers use this to avoid
/// rewriting a real terminal outcome in the task record.
#[derive(Debug, PartialEq, Eq)]
enum FailureMark {
    /// The job was non-terminal and now carries `failed`/`startup_failed`.
    Marked,
    /// The job already had a terminal status and was left untouched.
    AlreadyTerminal,
}

/// Records a startup failure. The original cause is always preserved; a
/// failure to write the state or the task record is appended, because leaving
/// a `starting`/`pending` record behind must be visible to the caller instead
/// of being reported as if the failure had been fully recorded.
async fn record_start_failure(
    root: &Path,
    session: &str,
    id: &str,
    directory: &Path,
    message: &str,
    mark_state: bool,
) -> anyhow::Error {
    let mut notes = Vec::new();
    // A setup failure has no state record to mark; an admission failure is
    // already terminal and only needs the task record to agree.
    let mut record_task = !mark_state;
    if mark_state {
        match mark_failed(directory, message) {
            Ok(FailureMark::Marked) => record_task = true,
            Ok(FailureMark::AlreadyTerminal) => {
                // The owner published a real terminal outcome. It is final in
                // both records, so a startup failure must not rewrite the task
                // record as `failed`. Every `mark_state` call site has already
                // killed or reaped its child, so no live owner can race this
                // check with a later terminal write.
                record_task = false;
            }
            Err(error) => {
                // Without a readable state we cannot prove the job is not
                // terminal; the task record is not overwritten either. The
                // state error below still tells the caller recording failed.
                notes.push(format!("{error:#}"));
                record_task = false;
            }
        }
    }
    if record_task {
        if let Err(error) = fail_record(root, session, id, message).await {
            notes.push(format!("{error:#}"));
        }
    }
    if notes.is_empty() {
        anyhow!("{message}")
    } else {
        anyhow!(
            "{message} (the failure could not be fully recorded: {})",
            notes.join("; ")
        )
    }
}

/// Writes `failed`/`startup_failed` unless the job already reached a terminal
/// state. Returns the stage that failed so the caller can report it alongside
/// the original startup error instead of losing it.
fn mark_failed(directory: &Path, message: &str) -> Result<FailureMark> {
    let state_path = directory.join("state.json");
    let mut state = load_state(directory).with_context(|| {
        format!(
            "Reading {} to record the ToolScript startup failure",
            state_path.display()
        )
    })?;
    if state.is_terminal() {
        // Completed/cancelled/failed is final; failure handling must not
        // overwrite an outcome the owner already published.
        return Ok(FailureMark::AlreadyTerminal);
    }
    state.status = "failed".into();
    state.finished_ms = Some(now_ms());
    state.termination = Some("startup_failed".into());
    state.error = Some(message.to_owned());
    save_atomic(&state_path, &state).with_context(|| {
        format!(
            "Persisting the failed ToolScript state in {}",
            state_path.display()
        )
    })?;
    Ok(FailureMark::Marked)
}

/// Marks the task record failed and returns any update failure: a start
/// failure must not silently leave the record `pending`.
async fn fail_record(root: &Path, session: &str, id: &str, message: &str) -> Result<()> {
    crate::tools::tasks::execute_for_session_async(
        "TaskUpdate",
        json!({"id": id, "status": "failed", "output": message}),
        root,
        session,
    )
    .await
    .with_context(|| format!("Updating the ToolScript task record {id} to failed"))?;
    Ok(())
}

fn read_bounded(path: &Path, limit: u64) -> Result<(String, bool)> {
    use std::io::Read as _;
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((String::new(), false))
        }
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    let mut bytes = Vec::new();
    file.take(limit).read_to_end(&mut bytes)?;
    Ok((
        String::from_utf8_lossy(&bytes).into_owned(),
        metadata.len() > limit,
    ))
}

fn result_value(directory: &Path) -> Option<Value> {
    let path = directory.join("result.json");
    let bytes = fs::read(&path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// One point-in-time answer for TaskGet. Page mode reads only the append-only
/// `output.log`; the final result was appended to that same stream by the
/// owner, so a bounded page can never hide an unbounded `result.json`.
fn snapshot(directory: &Path, id: &str, input: &Value, block: bool) -> Result<Value> {
    use crate::tools::output_page::{self, Cursor, PageRequest};
    let state = load_state(directory)?;
    let done = state.is_terminal();
    let request = PageRequest::parse(input)?;
    let mut value = json!({
        "task_id": id,
        "subject": "ToolScript",
        "status": state.status,
        "termination": state.termination,
        "error": state.error,
        "block": block,
        "retrieval_status": if done { "success" } else { "not_ready" },
        "result": Value::Null,
    });
    if let PageRequest::Page { cursor, max_bytes } = request {
        let path = directory.join("output.log");
        let offset = cursor.as_ref().map(Cursor::out).unwrap_or(0);
        let mut stream = output_page::open_stream(&path, offset)?;
        if let Some(cursor) = cursor {
            cursor.validate(id, "toolscript")?;
            cursor.validate_stream_at(
                offset,
                0,
                cursor.out_gen(),
                &output_page::stream_generation_of(&stream),
            )?;
        }
        let page = output_page::read_stream_page(stream.as_mut(), offset, max_bytes, done)?;
        let next = Cursor::new(
            id,
            "toolscript",
            page.generation.clone(),
            "none".into(),
            page.next,
            0,
        );
        value["stdout"] = json!(page.text);
        value["stderr"] = json!("");
        value["output"] = Value::Null;
        value["output_mode"] = json!("page");
        value["next_cursor"] = json!(next.encode());
        value["has_more"] = json!(page.truncated);
        value["output_truncated"] = json!(page.truncated);
        value["max_output_bytes"] = json!(max_bytes);
        return Ok(value);
    }

    let (stdout, truncated) = read_bounded(&directory.join("output.log"), SNAPSHOT_LIMIT)?;
    value["stdout"] = json!(stdout);
    value["stderr"] = json!("");
    value["output_truncated"] = json!(truncated);
    if done {
        // Bounded by the script result limit; a larger value is reported, not
        // silently truncated.
        let (text, oversized) = read_bounded(&directory.join("result.json"), RESULT_LIMIT)?;
        if oversized {
            value["result"] = json!({"truncated": true, "bytes": RESULT_LIMIT});
        } else if let Ok(parsed) = serde_json::from_str::<Value>(&text) {
            value["result"] = parsed;
        }
    }
    Ok(value)
}

/// TaskGet for a ToolScript job: bounded wait, `timeout:0` immediate, paging,
/// and a timeout that never cancels the script.
pub async fn read_job(root: &Path, session: &str, id: &str, input: &Value) -> Result<Value> {
    let directory = directory(root, id)?;
    let state = load_state(&directory).context("ToolScript job not found")?;
    ensure!(state.session == session, "Task belongs to another session");
    // A reopened session must not report a dead owner as running.
    if !state.is_terminal() {
        if let Some(recovered) = recover_job(&directory)? {
            let _ = recovered;
        }
    }
    let block = input
        .get("block")
        .map(|value| value.as_bool().context("block must be boolean"))
        .transpose()?
        .unwrap_or(false);
    let timeout = input
        .get("timeout")
        .map(|value| {
            value
                .as_u64()
                .context("TaskGet timeout must be an integer number of milliseconds")
        })
        .transpose()?
        .unwrap_or(30_000);
    ensure!(
        timeout <= 600_000,
        "TaskGet timeout must be 0..600000 milliseconds"
    );
    let wait = block && timeout > 0;
    let deadline = Instant::now() + Duration::from_millis(timeout);
    let mut last = snapshot(&directory, id, input, block)?;
    loop {
        if !wait || last["status"].as_str().is_some_and(is_terminal) {
            return Ok(last);
        }
        if Instant::now() >= deadline {
            last["retrieval_status"] = json!("timeout");
            return Ok(last);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        if load_state(&directory)
            .map(|state| state.is_terminal())
            .unwrap_or(false)
        {
            last = snapshot(&directory, id, input, block)?;
            return Ok(last);
        }
    }
}

/// TaskStop requests cancellation and reports it as requested, not done. The
/// owner writes the terminal state after its own cleanup.
pub fn stop_job(root: &Path, session: &str, id: &str) -> Result<Value> {
    let directory = directory(root, id)?;
    let mut state = load_state(&directory).context("ToolScript job not found")?;
    ensure!(state.session == session, "Task belongs to another session");
    if !state.is_terminal() && !owner_is_live(&directory) {
        recover_job(&directory)?;
        state = load_state(&directory)?;
    }
    let active = !state.is_terminal();
    if active {
        fs::write(directory.join("cancel"), b"cancel")?;
    }
    Ok(json!({
        "task_id": id,
        "subject": "ToolScript",
        "status": state.status,
        "cancellation_requested": active,
        "retrieval_status": if state.is_terminal() { "success" } else { "not_ready" },
        "message": if active {
            "Cancellation is requested, not confirmed. TaskGet reports the terminal state after the owner stops the script and its exact owned processes."
        } else {
            "The ToolScript job already reached a terminal state."
        },
    }))
}

/// Entry point called from the host binary **before** it starts its runtime.
pub fn run_worker_from_env() -> Result<bool> {
    let Some(directory) = std::env::var_os(WORKER_ENV) else {
        return Ok(false);
    };
    std::env::remove_var(WORKER_ENV);
    if tokio::runtime::Handle::try_current().is_ok() {
        // Building a runtime inside a runtime panics. A host that already has
        // one must call `run_worker_async` instead of this synchronous entry,
        // and the CLI dispatches it before `block_on`.
        return Err(anyhow!(
            "The ToolScript worker must be dispatched before the host runtime starts"
        ));
    }
    run_worker(Path::new(&directory))?;
    Ok(true)
}

/// The owner process: takes the lease, runs the script, writes one terminal
/// state and releases the lease.
pub fn run_worker(directory: &Path) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()
        .context("Starting the ToolScript worker runtime")?;
    runtime.block_on(run_worker_async(directory))
}

/// Async owner entry for a host that already owns a runtime.
pub async fn run_worker_async(directory: &Path) -> Result<()> {
    let Some(_lease) = OwnedLease::acquire(directory)? else {
        return Err(anyhow!("Another ToolScript owner already holds this job"));
    };
    let spec = load_spec(directory).context("Reading the ToolScript job spec")?;
    let mut state = load_state(directory).context("Reading the ToolScript job state")?;
    if state.is_terminal() {
        // A terminal outcome is final. A new owner must never turn it back into
        // `running` or replay the script.
        return Err(anyhow!(
            "ToolScript job already reached a terminal state ({})",
            state.status
        ));
    }
    state.status = RUNNING.into();
    state.owner_pid = Some(std::process::id());
    save_atomic(&directory.join("state.json"), &state)?;
    // The lease holder owns the record update, before any call can run. The
    // parent only observes, so a finished record cannot be moved back to
    // `running` by a late parent update.
    if let Err(error) = crate::tools::tasks::execute_for_session_async(
        "TaskUpdate",
        json!({"id": spec.id, "status": RUNNING}),
        Path::new(&spec.data_dir),
        &spec.session,
    )
    .await
    {
        let message = format!("Updating the ToolScript task record: {error:#}");
        finalize_failure(directory, &spec, &message).await;
        return Err(anyhow!(message));
    }

    match run_script(directory, &spec, &mut state).await {
        Ok(()) => Ok(()),
        Err(error) => {
            // An initialization failure is not an owner loss: record it and
            // finish the task truthfully instead of leaving `running` behind.
            finalize_failure(directory, &spec, &format!("{error:#}")).await;
            Err(error)
        }
    }
}

/// Records an owner-side failure that happened before or outside the script's
/// own outcome handling. Never leaves a non-terminal `running` record.
async fn finalize_failure(directory: &Path, spec: &JobSpec, message: &str) {
    let message = sanitize(message);
    if let Ok(mut current) = load_state(directory) {
        if !current.is_terminal() {
            current.status = "failed".into();
            current.termination = Some("initialization_failed".into());
            current.error = Some(message.clone());
            current.finished_ms = Some(now_ms());
            let _ = save_atomic(&directory.join("state.json"), &current);
            let _ = crate::tools::tasks::execute_for_session_async(
                "TaskUpdate",
                json!({"id": spec.id, "status": "failed", "output": message}),
                Path::new(&spec.data_dir),
                &spec.session,
            )
            .await;
        }
    }
}

async fn run_script(directory: &Path, spec: &JobSpec, state: &mut JobState) -> Result<()> {
    let data_dir = PathBuf::from(&spec.data_dir);
    let workspace = PathBuf::from(&spec.workspace);
    let provider = resolve_provider(&data_dir, spec)?;
    let permissions = crate::runtime::default_permissions();
    let db = crate::runtime::RuntimePaths::new(data_dir.clone())?.open_database()?;
    // The durable snapshot is read *before* the first call, never after one
    // already ran with a stale admission snapshot. Any required source that
    // cannot be read or parsed fails the job instead of dropping a stored deny.
    let mut snapshot = read_permission_snapshot(&data_dir, &workspace, &db, &spec.session)?;
    snapshot.mode = Some(
        snapshot
            .mode
            .or_else(|| crate::services::session_permission_mode::parse(&spec.permission_mode).ok())
            .unwrap_or(crate::permissions::PermissionMode::AskPermissions),
    );
    apply_permission_snapshot(&permissions, &spec.session, snapshot)?;
    // The same manager is refreshed below while the job runs, so a revocation
    // applied by the host takes effect on the next nested call.
    let policy = permissions.clone();

    let (tx, mut rx) = mpsc::channel::<EngineEvent>(1024);
    let mut executor = ToolLoopExecutor::new(
        provider,
        Vec::new(),
        None,
        8192,
        tx.clone(),
        spec.workspace.clone(),
        spec.reasoning.clone(),
    )
    .with_conv_id(spec.session.clone())
    .with_search_config_path(PathBuf::from(&spec.settings_path))
    .with_permission_manager(permissions)
    .with_loaded_tools(spec.loaded_tools.iter().cloned().collect());
    if let Some(allowed) = &spec.allowed_tools {
        executor = executor.with_allowed_tools(Some(allowed.clone()));
    }
    if let Some(exposed) = &spec.exposed_tools {
        executor = executor.with_exposed_tools(exposed.iter().cloned().collect());
    }
    // Hooks are part of the authority: a loading failure must fail the job
    // rather than silently running without them.
    let hooks = crate::services::tool_hooks::HookRunner::load(&data_dir, &workspace)
        .context("Loading ToolScript hooks")?;
    executor = executor.with_hooks(hooks);
    drop(tx);

    let cancellation = Cancellation::new();
    let persistence_error: std::sync::Arc<std::sync::Mutex<Option<String>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    // The first failure to re-read the durable policy. It stops the script and
    // is reported as the terminal cause instead of a user cancellation.
    let policy_error: std::sync::Arc<std::sync::Mutex<Option<String>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    let mailbox = executor.permission_mailbox_handle();
    let log_path = directory.join("output.log");
    let approvals = directory.join("approvals");
    let interactive = spec.interactive;
    let approvals_session = spec.session.clone();
    let persist = spec
        .assistant_id
        .as_ref()
        .map(|assistant| (data_dir.clone(), spec.session.clone(), assistant.clone()));
    let consumer_log = log_path.clone();
    let consumer_failure = persistence_error.clone();
    let consumer_cancel = cancellation.clone();
    // Calls the script really admitted, counted from the same event stream the
    // consumer records. A failure path must not report zero calls when the
    // script already ran some.
    let script_counts: std::sync::Arc<std::sync::Mutex<(u64, u64)>> =
        std::sync::Arc::new(std::sync::Mutex::new((0, 0)));
    let consumer_counts = script_counts.clone();
    let consumer = tokio::spawn(async move {
        let log_path = consumer_log;
        let mut rows: Vec<crate::db::message_repo::ToolCallRow> = Vec::new();
        let mut origins = ToolOrigins::new();
        let mut arguments: HashMap<String, String> = HashMap::new();
        let mut flushed: std::collections::HashSet<String> = std::collections::HashSet::new();
        // A history or log write that fails must stop further side effects and
        // make the job fail; it must never be reported as a success.
        let mut fail = |message: String| {
            let mut slot = consumer_failure
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if slot.is_none() {
                *slot = Some(message);
            }
            drop(slot);
            consumer_cancel.cancel();
        };
        while let Some(event) = rx.recv().await {
            {
                let mut counts = consumer_counts
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                match &event {
                    EngineEvent::ToolUseStart { .. } => counts.0 += 1,
                    EngineEvent::ToolUseDone { is_error, .. } => {
                        if !is_error {
                            counts.1 += 1;
                        }
                    }
                    _ => {}
                }
            }
            if let Err(error) = append_line(&log_path, &render(&event)) {
                fail(format!("Could not write the job log: {error}"));
            }
            if let EngineEvent::ToolPermission {
                request_id,
                tool_name,
                input,
                ..
            } = &event
            {
                // A worker cannot show a prompt. It either hands the request to
                // a live host through the durable approval directory, or denies
                // it explicitly. It never auto-allows and never waits forever.
                let mailbox = mailbox.clone();
                let approvals = approvals.clone();
                let session = approvals_session.clone();
                let (request_id, tool, input) =
                    (request_id.clone(), tool_name.clone(), input.clone());
                tokio::spawn(async move {
                    let response =
                        resolve_permission(&approvals, interactive, &session, &tool, &input).await;
                    let _ = append_line(
                        &approvals.join("log.txt"),
                        &format!("{request_id} {tool} -> {:?}\n", response_kind(&response)),
                    );
                    let _ = crate::native_engine::permission_response::respond(
                        &mailbox,
                        &request_id,
                        response,
                    )
                    .await;
                });
            }
            tool_history::apply_event(&mut rows, &mut origins, &mut arguments, &event);
            if let Some((data_dir, session, assistant)) = &persist {
                if let Err(error) =
                    flush_rows(data_dir, session, assistant, &rows, &origins, &mut flushed)
                {
                    fail(format!("Could not persist nested tool history: {error:#}"));
                }
            }
        }
    });

    let cancel_token = cancellation.clone();
    let cancel_dir = directory.to_path_buf();
    let watcher = tokio::spawn(async move {
        loop {
            if cancel_dir.join("cancel").exists() {
                cancel_token.cancel();
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    });
    let refresher = {
        let policy = policy.clone();
        let data_dir = data_dir.clone();
        let workspace = workspace.clone();
        let session = spec.session.clone();
        let token = cancellation.clone();
        let db = db.clone();
        let failure = policy_error.clone();
        let log_path = log_path.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = token.cancelled() => return,
                    _ = tokio::time::sleep(POLICY_REFRESH) => {}
                }
                // Every tick re-reads and re-applies the whole durable snapshot
                // strictly. Keeping the previous rules on a failed read would
                // keep using an allow the host may already have revoked, so a
                // failure stops the script instead. The period is not a
                // per-call guarantee: a call admitted in the same window can
                // already have run under the older policy.
                let refreshed = read_permission_snapshot(&data_dir, &workspace, &db, &session)
                    .and_then(|snapshot| apply_permission_snapshot(&policy, &session, snapshot));
                if let Err(error) = refreshed {
                    record_policy_failure(&failure, &token, &log_path, &error);
                    return;
                }
            }
        })
    };

    let outcome = executor
        .run_toolscript(
            &spec.source,
            &spec.args,
            &spec.parent_tool_use_id,
            &cancellation,
        )
        .await;
    watcher.abort();
    refresher.abort();
    drop(executor);
    if let Err(error) = consumer.await {
        // The consumer owns every event and its counters. A panic there means
        // the recorded outcome is incomplete, so report it instead of dropping
        // the join error.
        let mut slot = persistence_error
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if slot.is_none() {
            *slot = Some(format!("The ToolScript event consumer stopped: {error}"));
        }
    }
    // The refresher records its failure synchronously after its last await, so
    // the terminal decision must join it instead of racing the shared slot.
    if let Err(error) = watcher.await {
        if !error.is_cancelled() {
            tracing::warn!("The ToolScript cancel watcher stopped unexpectedly: {error}");
        }
    }
    if let Err(error) = refresher.await {
        if !error.is_cancelled() {
            record_policy_failure(
                &policy_error,
                &cancellation,
                &directory.join("output.log"),
                &anyhow!("The ToolScript permission refresher stopped unexpectedly: {error}"),
            );
        }
    }
    let persistence = persistence_error
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take();
    let policy_failure = policy_error
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take();

    // A refresh failure is the security-relevant cause: the script was stopped
    // because the durable policy could no longer be read. The same
    // `Cancellation` handle stops a user stop, so this must be reported as
    // `policy_refresh_failed` and never as `cancelled` or `completed`.
    if let Some(error) = policy_failure {
        let cleanup = crate::services::tool_hooks::await_process_cleanup(CLEANUP_TIMEOUT).await;
        let mut message = sanitize(&error);
        // The calls that already ran stay in `output.log` and the history; the
        // failure does not claim they were revoked.
        if let Some(persistence) = &persistence {
            message = format!(
                "{message}; the job log or history write also failed: {}",
                sanitize(persistence)
            );
        }
        if !cleanup.pending.is_empty() {
            message = format!(
                "{message}; these owned process groups were still alive after {}s: {:?}",
                CLEANUP_TIMEOUT.as_secs(),
                cleanup.pending
            );
        }
        // The script really ran these calls; report the counts instead of the
        // initial zeros. Prefer the engine's own totals when the script had
        // already returned; otherwise use the counts observed on the event
        // stream. They remain in output.log and the history and are not claimed
        // revoked.
        let (started, succeeded) = match &outcome {
            Ok(completed) => (completed.calls_started, completed.calls_succeeded),
            Err(_) => *script_counts
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        };
        state.calls_started = started;
        state.calls_succeeded = succeeded;
        state.status = "failed".into();
        state.termination = Some("policy_refresh_failed".into());
        state.error = Some(message.clone());
        let mut value = json!({
            "error": message,
            "policy_refresh_failed": true,
            "cancelled": false,
            "cleanup": {
                "verified": cleanup.verified,
                "pending": cleanup.pending,
            },
        });
        if let Ok(completed) = &outcome {
            // A script that returned in the same tick as the failed refresh
            // keeps its value visible instead of losing it.
            value["script_value"] = completed.value.clone();
        }
        save_atomic(&directory.join("result.json"), &value)?;
        finish(directory, state, "failed").await?;
        return Ok(());
    }

    // A history or log write that failed after a nested call ran makes the
    // recorded outcome incomplete, even when the script itself returned.
    if let Some(error) = persistence {
        state.status = "failed".into();
        state.termination = Some("persistence_failed".into());
        state.error = Some(sanitize(&error));
        if let Err(outcome) = outcome {
            let _ = outcome;
        }
        append_line(
            &log_path,
            &format!("persistence_failed {}\n", sanitize(&error)),
        );
        let value = json!({"error": sanitize(&error), "persistence_failed": true});
        save_atomic(&directory.join("result.json"), &value)?;
        let status = state.status.clone();
        finish(directory, state, &status).await?;
        return Ok(());
    }

    match outcome {
        Ok(outcome) => {
            let value = json!({
                "value": outcome.value,
                "logs": outcome.logs,
                "calls_started": outcome.calls_started,
                "calls_succeeded": outcome.calls_succeeded,
                "calls_failed": outcome.calls_failed,
                "calls_cancelled": outcome.calls_cancelled,
                "peak_concurrency": outcome.peak_concurrency,
                "abandoned_calls": outcome.abandoned_calls,
            });
            state.calls_started = outcome.calls_started;
            state.calls_succeeded = outcome.calls_succeeded;
            state.status = "completed".into();
            state.termination = Some("script_completed".into());
            save_atomic(&directory.join("result.json"), &value)?;
            append_line(&log_path, &format!("result {}\n", value))?;
            finish(directory, state, "completed").await?;
        }
        Err(error) => {
            let cancelled = cancellation.is_cancelled();
            let message = format!("{error:#}");
            append_line(
                &log_path,
                &format!(
                    "{} {}\n",
                    if cancelled { "cancelled" } else { "error" },
                    sanitize(&message)
                ),
            );
            if cancelled {
                // Drop only signals the owned groups. A terminal "cancelled"
                // is published only after the OS confirms they are gone; a
                // failed cleanup is reported truthfully instead.
                let cleanup =
                    crate::services::tool_hooks::await_process_cleanup(CLEANUP_TIMEOUT).await;
                if !cleanup.pending.is_empty() {
                    let detail = format!(
                        "The script was cancelled, but these owned process groups were still alive after {}s: {:?}",
                        CLEANUP_TIMEOUT.as_secs(),
                        cleanup.pending
                    );
                    state.status = "failed".into();
                    state.termination = Some("cleanup_incomplete".into());
                    state.error = Some(detail.clone());
                    append_line(&log_path, &format!("cleanup_incomplete {detail}\n"))?;
                } else {
                    state.status = "cancelled".into();
                    state.termination = Some(if cleanup.verified {
                        "task_stop".into()
                    } else {
                        // This platform cannot verify awaited cleanup; say so
                        // rather than claiming the processes are gone.
                        "task_stop_unverified".into()
                    });
                    state.error = None;
                }
                let value = json!({
                    "error": sanitize(&message),
                    "cancelled": state.status == "cancelled",
                    "cleanup": {
                        "verified": cleanup.verified,
                        "pending": cleanup.pending,
                    },
                });
                save_atomic(&directory.join("result.json"), &value)?;
            } else {
                state.status = "failed".into();
                state.termination = Some("script_error".into());
                state.error = Some(sanitize(&message));
                let value = json!({"error": sanitize(&message), "cancelled": false});
                save_atomic(&directory.join("result.json"), &value)?;
            }
            let status = state.status.clone();
            finish(directory, state, &status).await?;
        }
    }
    Ok(())
}

/// Resolves one nested permission prompt.
///
/// With a live host, the request is written to the durable approval directory
/// and the answer is awaited up to `APPROVAL_TIMEOUT`. Without one (background
/// job, `--print`, or a host that exited), the call fails closed: it is denied
/// with an explicit message, never auto-allowed and never left hanging.
async fn resolve_permission(
    approvals: &Path,
    interactive: bool,
    _session: &str,
    tool: &str,
    input: &Value,
) -> crate::native_engine::permission_response::PermissionResponse {
    use crate::native_engine::permission_response::PermissionResponse;
    let deny = |message: String| PermissionResponse::Deny {
        message,
        interrupt: false,
    };
    if !interactive {
        return deny(format!(
            "{tool} requires user approval, but this ToolScript job has no interactive host. The call was not auto-allowed."
        ));
    }
    if fs::create_dir_all(approvals).is_err() {
        return deny(format!(
            "{tool} requires user approval, but the approval channel is unavailable. The call was not auto-allowed."
        ));
    }
    let request = approvals.join(format!("{}.request.json", uuid::Uuid::new_v4()));
    let response_path = request.with_extension("response.json");
    if save_atomic(&request, &json!({"tool": tool, "input": input})).is_err() {
        return deny(format!(
            "{tool} requires user approval, but the request could not be recorded. The call was not auto-allowed."
        ));
    }
    let deadline = Instant::now() + APPROVAL_TIMEOUT;
    loop {
        if let Ok(bytes) = fs::read(&response_path) {
            if let Ok(value) = serde_json::from_slice::<Value>(&bytes) {
                return match value["decision"].as_str() {
                    Some("allow") => PermissionResponse::Allow {
                        updated_input: value.get("updated_input").cloned(),
                        commit: None,
                        note: value["note"].as_str().map(str::to_owned),
                    },
                    Some("deny") => deny(
                        value["message"]
                            .as_str()
                            .unwrap_or("The user denied this tool call")
                            .to_owned(),
                    ),
                    _ => deny(format!(
                        "{tool} approval response was invalid. The call was not auto-allowed."
                    )),
                };
            }
        }
        if Instant::now() >= deadline {
            return deny(format!(
                "{tool} needs user approval, but no host answered within {}s. The call was not auto-allowed.",
                APPROVAL_TIMEOUT.as_secs()
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn response_kind(
    response: &crate::native_engine::permission_response::PermissionResponse,
) -> &'static str {
    use crate::native_engine::permission_response::PermissionResponse;
    match response {
        PermissionResponse::Allow { .. } => "allow",
        PermissionResponse::AllowBashForSession { .. } => "allow_bash_session",
        PermissionResponse::AllowEditsForSession { .. } => "allow_edits_session",
        PermissionResponse::Deny { .. } => "deny",
        PermissionResponse::Text(_) => "text",
    }
}

async fn finish(directory: &Path, state: &mut JobState, status: &str) -> Result<()> {
    state.finished_ms = Some(now_ms());
    save_atomic(&directory.join("state.json"), state)
        .context("Writing the ToolScript terminal state")?;
    let spec = load_spec(directory).context("Reading the ToolScript job spec")?;
    let output = match status {
        "completed" => "ToolScript completed.".to_owned(),
        "cancelled" => "ToolScript was cancelled.".to_owned(),
        _ => state
            .error
            .clone()
            .unwrap_or_else(|| "ToolScript failed.".into()),
    };
    // A failing record update is visible instead of being swallowed; the job
    // state still carries the real script outcome.
    if let Err(error) = crate::tools::tasks::execute_for_session_async(
        "TaskUpdate",
        json!({"id": spec.id, "status": status, "output": output}),
        Path::new(&spec.data_dir),
        &spec.session,
    )
    .await
    {
        let note = format!("The task record could not be updated: {error:#}");
        append_line(
            &directory.join("output.log"),
            &format!("record_error {note}\n"),
        )?;
        if let Ok(mut current) = load_state(directory) {
            current.error = Some(match current.error {
                Some(existing) => format!("{existing}; {note}"),
                None => note,
            });
            save_atomic(&directory.join("state.json"), &current)?;
        }
    }
    Ok(())
}

fn resolve_provider(
    data_dir: &Path,
    spec: &JobSpec,
) -> Result<crate::native_engine::provider_manager::ResolvedProvider> {
    let mut managers = ProviderManager::new(data_dir.join("providers.json"));
    managers.load();
    let mut provider = managers
        .resolve_selected(&spec.provider_id, &spec.model)
        .context("The ToolScript provider/model is unavailable")?;
    if provider.provider.api_key.is_empty() {
        // Runtime-only channel; never written into the job files.
        if let Ok(key) = std::env::var(CHILD_KEY_ENV) {
            provider.provider.api_key = key;
        }
    }
    Ok(provider)
}

fn stored_session_rules_with(
    db: &crate::db::DbManager,
    session: &str,
) -> Result<HashMap<String, crate::permissions::rules::PermissionAction>> {
    let value = db.with_conn(|conn| -> Result<Option<String>> {
        let mut statement = conn.prepare("SELECT value FROM peripheral_settings WHERE key=?1")?;
        let mut rows = statement.query([format!("cli.permissions.{session}")])?;
        Ok(rows
            .next()?
            .map(|row| row.get::<_, String>(0))
            .transpose()?)
    })??;
    match value {
        // An unreadable rule set is a hard failure: defaulting to "no rules"
        // would silently drop stored deny rules and widen authority.
        Some(value) => {
            Ok(serde_json::from_str(&value)
                .context("Stored session permission rules are invalid")?)
        }
        None => Ok(HashMap::new()),
    }
}

/// One strict read of every durable permission source a job depends on.
///
/// The interactive CLI deliberately ignores a settings file it cannot parse.
/// A running job must not: that would silently drop a stored deny, or keep an
/// allow the user has already replaced. A missing file is normal; a present but
/// unreadable, malformed or non-object one is a failure.
#[derive(Debug)]
struct PermissionSnapshot {
    rules: HashMap<String, crate::permissions::rules::PermissionAction>,
    directories: Vec<PathBuf>,
    /// `None` when no durable mode is stored.
    mode: Option<crate::permissions::PermissionMode>,
}

fn read_permission_snapshot(
    data_dir: &Path,
    workspace: &Path,
    db: &crate::db::DbManager,
    session: &str,
) -> Result<PermissionSnapshot> {
    // One parse of each source feeds the merge. The lenient CLI loader cannot
    // be used after a separate validation read: a file that changed in between
    // would be skipped as "no rules", dropping a deny.
    let loaded =
        crate::services::permission_updates::load_settings_strict(data_dir, workspace, true)
            .context("Reading the ToolScript permission settings")?;
    let mut rules = loaded.rules;
    // A failure to read stored rules must fail the job, not silently drop a
    // deny rule and run with wider authority.
    let stored = stored_session_rules_with(db, session)
        .context("Reading the session permission rules for the ToolScript job")?;
    for (pattern, action) in stored {
        // A stored deny always wins; everything else follows the stored rules.
        match rules.get(&pattern) {
            Some(crate::permissions::rules::PermissionAction::Deny) => {}
            _ => {
                rules.insert(pattern, action);
            }
        }
    }
    // The durable mode is authoritative; `None` means it is absent, which the
    // caller resolves (initial load) or leaves in force (refresh).
    let mode = crate::services::session_permission_mode::load(db, session)
        .context("Reading the durable session permission mode for the ToolScript job")?;
    Ok(PermissionSnapshot {
        rules,
        directories: loaded.directories,
        mode,
    })
}

/// Applies one snapshot. Rules and directories are always replaced; the mode is
/// replaced only when the snapshot carries one, so a refresh that finds no
/// durable mode keeps the mode already in force instead of re-deriving it from
/// the launch snapshot (which could widen authority).
fn apply_permission_snapshot(
    policy: &crate::permissions::PermissionManager,
    session: &str,
    snapshot: PermissionSnapshot,
) -> Result<()> {
    // `set_launch_rules` validates every pattern before it changes anything, so
    // a rejected rule set leaves the previous policy untouched.
    policy
        .set_launch_rules(session, snapshot.rules)
        .context("Applying the ToolScript permission rules")?;
    policy.set_session_directories(session, snapshot.directories);
    if let Some(mode) = snapshot.mode {
        policy.set_mode(mode);
    }
    Ok(())
}

/// Records the first refresh failure, stops the script and writes the cause to
/// the job log. Cancellation is requested before the log line, so a reader that
/// sees the line knows no further internal call will be admitted.
fn record_policy_failure(
    slot: &std::sync::Mutex<Option<String>>,
    cancellation: &Cancellation,
    log_path: &Path,
    error: &anyhow::Error,
) {
    let message = format!("{error:#}");
    {
        let mut current = slot.lock().unwrap_or_else(|error| error.into_inner());
        if current.is_none() {
            *current = Some(message.clone());
        }
    }
    cancellation.cancel();
    if let Err(error) = append_line(
        log_path,
        &format!("policy_refresh_failed {}\n", sanitize(&message)),
    ) {
        // The slot is authoritative for the terminal state; the log failure is
        // still reported with the cause instead of being dropped.
        let mut current = slot.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(existing) = current.as_mut() {
            *existing = format!("{existing}; the job log could not record it: {error:#}");
        }
    }
}

fn sanitize(message: &str) -> String {
    message.chars().take(4096).collect()
}

fn append_line(path: &Path, text: &str) -> Result<()> {
    use std::io::Write as _;
    let mut file = File::options().create(true).append(true).open(path)?;
    file.write_all(text.as_bytes())?;
    file.flush()?;
    Ok(())
}

/// Renders one event into the append-only log. Only the fields a reader needs
/// are kept; no message content or credential is copied.
fn render(event: &EngineEvent) -> String {
    match event {
        EngineEvent::NestedToolCall {
            tool_use_id,
            parent_tool_use_id,
            sequence,
        } => format!(
            "nested {} parent={} seq={}\n",
            tool_use_id, parent_tool_use_id, sequence
        ),
        EngineEvent::ToolUseStart {
            tool_use_id,
            tool_name,
            tool_input,
            ..
        } => format!("start {} {} {}\n", tool_use_id, tool_name, tool_input),
        EngineEvent::ToolUseDone {
            tool_use_id,
            tool_name,
            output,
            is_error,
            ..
        } => format!(
            "done {} {} error={}\n{}\n",
            tool_use_id, tool_name, is_error, output
        ),
        _ => String::new(),
    }
}

/// Persists every observed nested row that has an origin. Row + origin are one
/// transaction; repeating this is idempotent.
fn flush_rows(
    data_dir: &Path,
    session: &str,
    assistant: &str,
    rows: &[crate::db::message_repo::ToolCallRow],
    origins: &ToolOrigins,
    flushed: &mut std::collections::HashSet<String>,
) -> Result<()> {
    let paths = crate::runtime::RuntimePaths::new(data_dir.to_path_buf())?;
    let db = paths.open_database()?;
    db.with_conn(|conn| -> Result<()> {
        for row in rows {
            let Some((parent, sequence)) = origins.get(&row.id) else {
                continue;
            };
            if flushed.contains(&row.id) && row.output.is_none() {
                continue;
            }
            let qualified = format!("{assistant}:{}", row.id);
            let parent = format!("{assistant}:{parent}");
            crate::db::message_repo::save_nested_tool_row(
                conn,
                assistant,
                &crate::db::message_repo::ToolCallRow {
                    id: qualified,
                    message_id: assistant.to_owned(),
                    ..row.clone()
                },
                &parent,
                *sequence as i64,
            )?;
            flushed.insert(row.id.clone());
        }
        Ok(())
    })??;
    let _ = session;
    Ok(())
}

#[cfg(test)]
mod j1_tests {
    use super::*;

    fn spec(directory: &Path) -> JobSpec {
        JobSpec {
            id: directory
                .file_name()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            session: uuid::Uuid::new_v4().to_string(),
            workspace: directory.to_string_lossy().into_owned(),
            assistant_id: None,
            parent_tool_use_id: "inv-1".into(),
            provider_id: "fixture".into(),
            model: "fixture".into(),
            source: "return 1;".into(),
            args: Value::Null,
            data_dir: directory.to_string_lossy().into_owned(),
            settings_path: directory.join("config.json").to_string_lossy().into_owned(),
            loaded_tools: Vec::new(),
            reasoning: ReasoningMode::Standard,
            permission_mode: "ask".into(),
            allowed_tools: None,
            exposed_tools: None,
            source_sha256: String::new(),
            interactive: false,
        }
    }

    fn job_dir(root: &Path, id: &str) -> PathBuf {
        let directory = root.join("toolscript-jobs").join(id);
        fs::create_dir_all(&directory).unwrap();
        directory
    }

    fn write_job(directory: &Path, status: &str, termination: Option<&str>) {
        let spec = spec(directory);
        save_atomic(&directory.join("spec.json"), &spec).unwrap();
        let mut state = JobState::new(&spec);
        state.id = spec.id.clone();
        state.status = status.into();
        state.termination = termination.map(str::to_owned);
        save_atomic(&directory.join("state.json"), &state).unwrap();
    }

    /// A spec accepted by `start_job`: a fresh session UUID (TaskCreate parses
    /// it) and the temp directory as both data dir and workspace.
    fn start_spec(root: &Path) -> JobSpec {
        let mut spec = spec(root);
        spec.session = uuid::Uuid::new_v4().to_string();
        spec.workspace = root.to_string_lossy().into_owned();
        spec.data_dir = root.to_string_lossy().into_owned();
        spec.settings_path = root.join("config.json").to_string_lossy().into_owned();
        spec
    }

    fn only_job_dir(root: &Path) -> PathBuf {
        let mut entries: Vec<PathBuf> = fs::read_dir(root.join("toolscript-jobs"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        assert_eq!(entries.len(), 1, "expected one job directory: {entries:?}");
        entries.pop().unwrap()
    }

    /// Serializes the process-wide `WORKER_EXE_ENV` override between tests.
    static WORKER_EXE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Runs the real `start_job` path against a controlled owner executable and
    /// restores the environment before returning.
    async fn start_with_worker_override(
        root: &Path,
        spec: JobSpec,
        worker: &Path,
    ) -> (anyhow::Result<StartOutcome>, JobSpec) {
        let _guard = WORKER_EXE_LOCK
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous = std::env::var_os(WORKER_EXE_ENV);
        std::env::set_var(WORKER_EXE_ENV, worker);
        let result = start_job(root, spec.clone()).await;
        match previous {
            Some(value) => std::env::set_var(WORKER_EXE_ENV, value),
            None => std::env::remove_var(WORKER_EXE_ENV),
        }
        (result, spec)
    }

    #[cfg(unix)]
    fn fake_worker(root: &Path, script: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let path = root.join("fake-toolscript-worker");
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn admission_accepts_a_real_terminal_outcome_and_rejects_a_start_failure() {
        let root = tempfile::tempdir().unwrap();
        let directory = job_dir(root.path(), "job");
        write_job(&directory, "running", None);
        assert_eq!(
            classify_admission(&load_state(&directory).unwrap()),
            Admission::Running
        );
        write_job(&directory, "completed", Some("script_completed"));
        assert_eq!(
            classify_admission(&load_state(&directory).unwrap()),
            Admission::Terminal
        );
        write_job(&directory, "failed", Some("script_error"));
        assert_eq!(
            classify_admission(&load_state(&directory).unwrap()),
            Admission::Terminal
        );
        write_job(&directory, "failed", Some("startup_failed"));
        assert_eq!(
            classify_admission(&load_state(&directory).unwrap()),
            Admission::Failed
        );
        write_job(&directory, "interrupted", Some("owner_lost"));
        assert_eq!(
            classify_admission(&load_state(&directory).unwrap()),
            Admission::Failed
        );
    }

    #[test]
    fn recovery_keeps_a_fresh_starting_job_and_interrupts_an_abandoned_one() {
        let root = tempfile::tempdir().unwrap();
        let directory = job_dir(root.path(), "fresh");
        write_job(&directory, "starting", None);
        // The spawn window: no owner has taken the lease yet, but the start is
        // still inside the admission handshake.
        assert!(recover_job(&directory).unwrap().is_none());

        let old = job_dir(root.path(), "old");
        write_job(&old, "starting", None);
        let mut state = load_state(&old).unwrap();
        state.started_ms = now_ms().saturating_sub(STARTING_GRACE_MS + 1);
        save_atomic(&old.join("state.json"), &state).unwrap();
        assert_eq!(
            recover_job(&old).unwrap().map(|state| state.status),
            Some("interrupted".to_owned())
        );
    }

    #[test]
    fn recovery_leaves_a_job_whose_owner_holds_the_lease() {
        let root = tempfile::tempdir().unwrap();
        let directory = job_dir(root.path(), "live");
        write_job(&directory, "running", None);
        let lease = OwnedLease::acquire(&directory).unwrap();
        assert!(lease.is_some());
        assert!(recover_job(&directory).unwrap().is_none());
        assert_eq!(load_state(&directory).unwrap().status, "running");
    }

    #[tokio::test]
    async fn a_worker_refuses_a_terminal_or_cancelled_job_without_overwriting_it() {
        let root = tempfile::tempdir().unwrap();
        for status in ["completed", "cancelled", "failed"] {
            let directory = job_dir(root.path(), status);
            write_job(&directory, status, Some("terminal"));
            let error = run_worker_async(&directory)
                .await
                .expect_err("terminal must not restart");
            assert!(format!("{error}").contains("terminal"), "{error}");
            assert_eq!(load_state(&directory).unwrap().status, status);
        }
    }

    #[tokio::test]
    async fn a_worker_refuses_a_job_another_owner_already_holds() {
        let root = tempfile::tempdir().unwrap();
        let directory = job_dir(root.path(), "held");
        write_job(&directory, "starting", None);
        let lease = OwnedLease::acquire(&directory)
            .unwrap()
            .expect("test takes the lease");
        let error = run_worker_async(&directory)
            .await
            .expect_err("second owner must be refused");
        assert!(
            format!("{error}").contains("Another ToolScript owner"),
            "{error}"
        );
        drop(lease);
    }

    #[tokio::test]
    async fn an_unreadable_state_is_an_explicit_error_not_silent_success() {
        let root = tempfile::tempdir().unwrap();
        let directory = job_dir(root.path(), "broken");
        write_job(&directory, "starting", None);
        fs::remove_file(directory.join("state.json")).unwrap();
        fs::create_dir(directory.join("state.json")).unwrap();
        assert!(run_worker_async(&directory).await.is_err());
    }

    /// A: a plain start failure writes `failed` and the caller still gets the
    /// original reason, with nothing appended that claims a repair problem.
    #[tokio::test]
    async fn a_start_failure_records_failed_state_and_preserves_the_original_cause() {
        let root = tempfile::tempdir().unwrap();
        let spec = start_spec(root.path());
        let session = spec.session.clone();
        let missing = root.path().join("missing-owner");
        let (result, _) = start_with_worker_override(root.path(), spec, &missing).await;
        let error = result
            .err()
            .expect("a missing owner executable must fail the start");
        let text = format!("{error:#}");
        assert!(text.contains("Starting the ToolScript owner"), "{text}");
        assert!(!text.contains("could not be fully recorded"), "{text}");

        let directory = only_job_dir(root.path());
        let state = load_state(&directory).unwrap();
        assert_eq!(state.status, "failed");
        assert_eq!(state.termination.as_deref(), Some("startup_failed"));
        assert!(
            state
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("Starting the ToolScript owner"),
            "{:?}",
            state.error
        );
        let task = crate::tools::tasks::execute_for_session(
            "TaskGet",
            json!({"task_id": state.id}),
            root.path(),
            &session,
        )
        .unwrap();
        assert_eq!(task["status"], "failed");
    }

    /// B: the state write really fails on the filesystem. The user sees the
    /// start cause and the persistence error, and the record is not presented
    /// as repaired.
    #[cfg(unix)]
    #[tokio::test]
    async fn b_state_persistence_failure_is_reported_without_claiming_a_repair() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let worker = fake_worker(
            root.path(),
            "#!/bin/sh\nchmod 0555 \"$DEEPSEEK_CODE_TOOLSCRIPT_JOB\"\nexit 1\n",
        );
        let spec = start_spec(root.path());
        let session = spec.session.clone();
        let (result, _) = start_with_worker_override(root.path(), spec, &worker).await;
        let error = result
            .err()
            .expect("the owner exited without admitting the job");
        let directory = only_job_dir(root.path());
        // Restore writability immediately so the failure does not leak into
        // cleanup; the assertions below run against the captured outcome.
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();

        let text = format!("{error:#}");
        assert!(text.contains("exited during admission"), "{text}");
        assert!(
            text.contains("the failure could not be fully recorded"),
            "{text}"
        );
        assert!(
            text.contains("Persisting the failed ToolScript state"),
            "{text}"
        );
        let state = load_state(&directory).unwrap();
        assert_eq!(
            state.status, "starting",
            "a failed write must not look recorded"
        );
        assert!(state.termination.is_none(), "{:?}", state.termination);
        // We could not prove the state is non-terminal, so the task record is
        // not rewritten either; both records honestly remain non-terminal.
        let task = crate::tools::tasks::execute_for_session(
            "TaskGet",
            json!({"task_id": state.id}),
            root.path(),
            &session,
        )
        .unwrap();
        assert_eq!(task["status"], "pending");
    }

    /// C: the task-record update fails independently of the state write. The
    /// specific failure is visible and the record is not claimed updated.
    #[cfg(unix)]
    #[tokio::test]
    async fn c_task_record_failure_is_reported_alongside_the_start_cause() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        let spec = start_spec(root.path());
        let session = spec.session.clone();
        let tasks = root.path().join("tasks");
        let database = tasks.join(format!("{session}.sqlite3"));
        let worker = fake_worker(
            root.path(),
            &format!(
                "#!/bin/sh\nchmod 0555 \"{}\"\nchmod 0444 \"{}\"\nexit 1\n",
                tasks.display(),
                database.display()
            ),
        );
        let (result, _) = start_with_worker_override(root.path(), spec, &worker).await;
        let error = result
            .err()
            .expect("the owner exited without admitting the job");
        let directory = only_job_dir(root.path());
        fs::set_permissions(&tasks, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&database, fs::Permissions::from_mode(0o644)).unwrap();

        let text = format!("{error:#}");
        assert!(text.contains("exited during admission"), "{text}");
        assert!(
            text.contains("Updating the ToolScript task record"),
            "{text}"
        );
        let state = load_state(&directory).unwrap();
        assert_eq!(state.status, "failed");
        assert_eq!(state.termination.as_deref(), Some("startup_failed"));
        let task = crate::tools::tasks::execute_for_session(
            "TaskGet",
            json!({"task_id": state.id}),
            root.path(),
            &session,
        )
        .unwrap();
        assert_eq!(
            task["status"], "pending",
            "a failed update must not be claimed as done"
        );
    }

    /// D: after a spawned owner fails, the exact child is reaped and the
    /// failure adds no script-side files.
    #[cfg(unix)]
    #[tokio::test]
    async fn d_a_spawned_failed_child_is_reaped_without_extra_files() {
        let root = tempfile::tempdir().unwrap();
        let pid_file = root.path().join("worker.pid");
        let worker = fake_worker(
            root.path(),
            &format!("#!/bin/sh\necho $$ > \"{}\"\nexit 1\n", pid_file.display()),
        );
        let spec = start_spec(root.path());
        let (result, _) = start_with_worker_override(root.path(), spec, &worker).await;
        assert!(result.is_err());
        let directory = only_job_dir(root.path());
        let state = load_state(&directory).unwrap();
        assert_eq!(state.status, "failed");
        #[cfg(target_os = "linux")]
        {
            let pid = fs::read_to_string(&pid_file).unwrap();
            let pid = pid.trim();
            assert!(!pid.is_empty());
            assert!(
                !Path::new(&format!("/proc/{pid}")).exists(),
                "the failed owner {pid} was not reaped"
            );
        }
        assert!(!directory.join("result.json").exists());
        assert!(!directory.join("output.log").exists());
    }

    /// D: failure recording must leave an already terminal state and its real
    /// task record byte-for-byte untouched. `start_job` cannot deterministically
    /// reach the failure branch with a terminal state (the owner writes it and
    /// the admission loop accepts it), so this drives the same recorder that
    /// every `start_job` failure path uses, with a real task row.
    #[tokio::test]
    async fn d_failure_recording_keeps_a_terminal_state_and_its_task_record() {
        let root = tempfile::tempdir().unwrap();
        for status in ["completed", "cancelled"] {
            let session = uuid::Uuid::new_v4().to_string();
            let task = crate::tools::tasks::execute_for_session(
                "TaskCreate",
                json!({"subject": format!("terminal {status}")}),
                root.path(),
                &session,
            )
            .unwrap();
            let id = task["id"].as_str().unwrap().to_owned();
            let updated = crate::tools::tasks::execute_for_session(
                "TaskUpdate",
                json!({"id": id, "status": status, "output": "owner outcome"}),
                root.path(),
                &session,
            )
            .unwrap();
            assert_eq!(updated["status"], status);

            let directory = job_dir(root.path(), &id);
            let mut spec = start_spec(root.path());
            spec.id = id.clone();
            spec.session = session.clone();
            save_atomic(&directory.join("spec.json"), &spec).unwrap();
            let mut state = JobState::new(&spec);
            state.status = status.into();
            state.termination = Some("owner_terminal".into());
            state.finished_ms = Some(now_ms());
            save_atomic(&directory.join("state.json"), &state).unwrap();

            let state_before = fs::read(directory.join("state.json")).unwrap();
            let task_before = crate::tools::tasks::execute_for_session(
                "TaskGet",
                json!({"task_id": id}),
                root.path(),
                &session,
            )
            .unwrap();

            let error = record_start_failure(
                root.path(),
                &session,
                &id,
                &directory,
                "a late startup failure",
                true,
            )
            .await;
            assert_eq!(format!("{error}"), "a late startup failure");

            assert_eq!(
                fs::read(directory.join("state.json")).unwrap(),
                state_before,
                "state.json must not change for a terminal job"
            );
            let task_after = crate::tools::tasks::execute_for_session(
                "TaskGet",
                json!({"task_id": id}),
                root.path(),
                &session,
            )
            .unwrap();
            assert_eq!(task_after, task_before, "the task record must not change");
            assert_eq!(task_after["status"], status);
        }
    }
}

#[cfg(test)]
mod j2_tests {
    use super::*;

    /// A data directory with the real schema the job opens.
    fn database(root: &Path) -> std::sync::Arc<crate::db::DbManager> {
        crate::runtime::RuntimePaths::new(root.to_path_buf())
            .unwrap()
            .open_database()
            .unwrap()
    }

    fn set_durable(db: &crate::db::DbManager, key: &str, value: &str) {
        db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO peripheral_settings(key,value) VALUES(?1,?2) \
                 ON CONFLICT(key) DO UPDATE SET value=excluded.value",
                [key, value],
            )
        })
        .unwrap()
        .unwrap();
    }

    /// A deny that the user has since written must beat the old stored allow,
    /// and a source that is simply absent is not an error.
    #[test]
    fn a_stored_deny_survives_the_merge_and_a_missing_source_is_not_a_failure() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let db = database(root.path());
        let session = "session";
        fs::write(
            root.path().join("settings.json"),
            json!({"permissions": {"allow": ["Bash"], "deny": ["Bash"]}}).to_string(),
        )
        .unwrap();
        // The stored rules still carry the allow that was just revoked.
        set_durable(
            &db,
            &format!("cli.permissions.{session}"),
            r#"{"Bash":"Allow"}"#,
        );

        let snapshot = read_permission_snapshot(root.path(), &workspace, &db, session)
            .expect("a readable snapshot");
        assert_eq!(
            snapshot.rules.get("Bash"),
            Some(&crate::permissions::rules::PermissionAction::Deny)
        );
        // No durable mode row: absent, not a failure.
        assert_eq!(snapshot.mode, None);
    }

    #[test]
    fn a_corrupt_session_rule_source_is_a_staged_failure() {
        let root = tempfile::tempdir().unwrap();
        let db = database(root.path());
        set_durable(&db, "cli.permissions.s1", "{not valid json");
        let error = read_permission_snapshot(root.path(), root.path(), &db, "s1")
            .expect_err("a corrupt rule source must fail");
        let text = format!("{error:#}");
        assert!(
            text.contains("Reading the session permission rules for the ToolScript job"),
            "{text}"
        );
    }

    #[test]
    fn a_corrupt_durable_mode_is_a_staged_failure() {
        let root = tempfile::tempdir().unwrap();
        let db = database(root.path());
        set_durable(&db, "cli.permission-mode.s2", "definitely-not-a-mode");
        let error = read_permission_snapshot(root.path(), root.path(), &db, "s2")
            .expect_err("a corrupt durable mode must fail");
        let text = format!("{error:#}");
        assert!(
            text.contains("Reading the durable session permission mode for the ToolScript job"),
            "{text}"
        );
    }

    /// `load_settings` skips a malformed file for interactive use; the job must
    /// not, or a stored deny would silently disappear.
    #[test]
    fn a_malformed_settings_file_fails_but_a_missing_one_does_not() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir_all(&workspace).unwrap();
        let db = database(root.path());
        assert!(read_permission_snapshot(root.path(), &workspace, &db, "s3").is_ok());
        fs::write(root.path().join("settings.json"), "{\"permissions\": ").unwrap();
        let error = read_permission_snapshot(root.path(), &workspace, &db, "s3")
            .expect_err("a malformed settings file must fail");
        let text = format!("{error:#}");
        assert!(
            text.contains("Reading the ToolScript permission settings"),
            "{text}"
        );
    }

    /// A rejected rule set must not clear or weaken what is already applied.
    #[test]
    fn a_rejected_rule_set_leaves_the_previous_policy_in_place() {
        use crate::permissions::rules::PermissionAction;
        let policy = crate::runtime::default_permissions();
        policy
            .set_launch_rules(
                "s4",
                HashMap::from([("Bash".to_owned(), PermissionAction::Allow)]),
            )
            .unwrap();
        let snapshot = PermissionSnapshot {
            rules: HashMap::from([("Bash(".to_owned(), PermissionAction::Allow)]),
            directories: Vec::new(),
            mode: None,
        };
        let error = apply_permission_snapshot(&policy, "s4", snapshot)
            .expect_err("an invalid pattern must fail the apply");
        assert!(format!("{error:#}").contains("Applying the ToolScript permission rules"));
        assert_eq!(
            policy.launch_rules("s4").get("Bash"),
            Some(&PermissionAction::Allow),
            "a rejected rule set must not clear the applied rules"
        );
    }
}
