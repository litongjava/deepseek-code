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

pub const WORKER_ENV: &str = "CLAUDE_RUST_BACKGROUND_JOB";
pub const CONTROL_ENV: &str = "CLAUDE_RUST_BACKGROUND_CONTROL";

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
    // Windows readers/antivirus can briefly deny replacement of a live state
    // file. Retry the same complete temporary file; never expose partial JSON.
    for attempt in 0..=20 {
        match temporary.persist(path) {
            Ok(_) => return Ok(()),
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
    let bytes = fs::read(&path).with_context(|| format!("Reading task state {}", path.display()))?;
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
    let id = uuid::Uuid::new_v4().to_string();
    let dir = directory(root, &id)?;
    fs::create_dir_all(&dir)?;
    let mut job = Job {
        id,
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
    };
    let mut claim = if foreground {
        Some(SessionClaim::acquire(root, session, &job.id)?)
    } else {
        None
    };
    save(&dir.join("launch.json"), &launch)?;
    save(&dir.join("state.json"), &job)?;
    let mut command = Command::new(supervisor);
    command
        .envs(environment.iter().map(|(key, value)| (key, value)))
        .env(WORKER_ENV, &dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
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
            // Reap when this host remains alive; dropping this thread on host exit
            // does not terminate the independently owned supervisor process.
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(error) => {
            job.status = "failed".into();
            job.error = Some(error.to_string());
            job.finished_ms = Some(now());
            save(&dir.join("state.json"), &job)?;
            return Err(error).context("Starting background supervisor");
        }
    }
    Ok(job)
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

fn run_worker(dir: &Path) -> Result<()> {
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
            Err(error) if error.downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) => continue,
            Err(error) => return Err(error),
        };
        if parent.map_or(true, |id| job.parent_session_id == id) {
            jobs.push(observed(job));
        }
    }
    jobs.sort_by_key(|job| job.started_ms);
    Ok(jobs)
}
pub fn get(root: &Path, id: &str) -> Result<Job> {
    Ok(observed(read_job(&directory(root, id)?)?))
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
    match fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
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
        let task = tokio::spawn(async move { wait_for_answer(&path, &request, "permission", "Allow?", None).await });
        tokio::time::timeout(Duration::from_secs(3), async {
            while !dir.path().join("pending-question.json").exists() { tokio::time::sleep(Duration::from_millis(10)).await; }
        }).await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(!dir.path().join("pending-question.json").exists());
        let guard = PendingAnswerCleanup { dir:dir.path().into(), request:id.clone() };
        save(&dir.path().join("pending-question.json"), &PendingQuestion {request_id:"next".into(),question:"Next?".into(),kind:"permission".into(),tool:None}).unwrap();
        fs::write(response_path(dir.path(), &id).unwrap(), b"late").unwrap();
        drop(guard);
        let pending: PendingQuestion = serde_json::from_slice(&fs::read(dir.path().join("pending-question.json")).unwrap()).unwrap();
        assert_eq!(pending.request_id, "next");
        assert!(!response_path(dir.path(), &id).unwrap().exists());
    }
}
impl Drop for PendingAnswerCleanup {
    fn drop(&mut self) {
        let pending = self.dir.join("pending-question.json");
        if fs::read(&pending).ok().and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
            .is_some_and(|value| value["request_id"] == self.request) {
            let _ = fs::remove_file(pending);
        }
        if let Ok(response) = response_path(&self.dir, &self.request) { let _ = fs::remove_file(response); }
    }
}

async fn wait_for_answer(dir: &Path, request: &str, kind: &str, question: &str, tool: Option<serde_json::Value>) -> Result<String> {
    let _cleanup = PendingAnswerCleanup { dir: dir.to_path_buf(), request: request.into() };
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
                let pending: PendingQuestion = serde_json::from_slice(
                    &fs::read(dir.join("pending-question.json"))?,
                )?;
                ensure!(pending.request_id == request, "Background question was superseded");
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
    let mut file = fs::File::open(path)?;
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
            Some(format!(
                "Owned child {execution_session} ended with status {}. Its transcript remains in the child session; inspect /tasks show {} and /list-agents inbox for the result. {}",
                job.status, job.id, job.error.as_deref().unwrap_or("")
            ))
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
    #[test]
    fn discovery_tolerates_concurrent_unpublished_jobs_but_reports_bad_state() {
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
        fs::write(directory(root.path(), &new_id).unwrap().join("state.json"), b"invalid").unwrap();
        assert!(format!("{:#}", list(root.path(), None).unwrap_err()).contains("Parsing task state"));
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
        assert!(wait_for_answer(&dir, "request", "permission", "Allow?", None)
            .await
            .is_err());
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
}
