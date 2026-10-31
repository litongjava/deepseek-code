//! Persistent session ownership, independent of individual model-turn jobs.
//! Provider credentials cross a local IPC stream in memory, never a job file.
mod recovery;
use super::background_sessions::{self as jobs, Job, SessionClaim};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

pub const SUPERVISOR_ENV: &str = "CLAUDE_RUST_SESSION_SUPERVISOR";
pub const HOST_ENV: &str = "CLAUDE_RUST_SESSION_HOST";
const MAX_FRAME: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Worker {
    pub id: String,
    pub session_id: String,
    pub root: PathBuf,
    pub workspace: PathBuf,
    pub executable: PathBuf,
    pub endpoint: String,
    pub status: String,
    pub supervisor_pid: Option<u32>,
    pub child_pid: Option<u32>,
    pub heartbeat_ms: u64,
    pub active_job: Option<String>,
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    Ping,
    RuntimeStatus,
    SetPermissionMode { job_id: String, mode: String },
    GetPermissionMode { job_id: String },
    Submit {
        id: String,
        prompt: String,
        arguments: Vec<String>,
        environment: Vec<(String, String)>,
    },
    Shutdown,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Reply {
    pub ok: bool,
    pub job_id: Option<String>,
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_rules: Option<std::collections::HashMap<String, crate::permissions::rules::PermissionAction>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_status: Option<super::runtime_status::RuntimeStatus>,
}
impl Reply {
    pub fn ok(id: Option<String>) -> Self {
        Self {
            ok: true,
            job_id: id,
            error: None,
            permission_mode: None,
            permission_session_id: None,
            launch_rules: None,
            runtime_status: None,
        }
    }
    pub fn error(error: impl ToString) -> Self {
        Self {
            ok: false,
            job_id: None,
            error: Some(error.to_string()),
            permission_mode: None,
            permission_session_id: None,
            launch_rules: None,
            runtime_status: None,
        }
    }
}

pub fn directory(worker: &Worker) -> PathBuf {
    worker
        .root
        .join("session-workers")
        .join(&worker.session_id)
        .join(&worker.id)
}
fn worker_path(root: &Path, session: &str) -> Result<PathBuf> {
    ensure!(
        uuid::Uuid::parse_str(session).is_ok(),
        "Invalid session worker ID"
    );
    Ok(root.join("session-workers").join(session))
}
pub fn read_worker(dir: &Path) -> Result<Worker> {
    Ok(serde_json::from_slice(&fs::read(dir.join("worker.json"))?)?)
}
fn current(root: &Path, session: &str) -> Result<Option<Worker>> {
    let base = worker_path(root, session)?;
    let pointer = match fs::read(base.join("current.json")) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let id: String = serde_json::from_slice(&pointer)?;
    ensure!(uuid::Uuid::parse_str(&id).is_ok(), "Invalid worker pointer");
    Ok(Some(read_worker(&base.join(id))?))
}

pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}
pub type LocalStream = Box<dyn Stream>;

#[cfg(windows)]
pub struct Listener {
    name: String,
    pipe: tokio::net::windows::named_pipe::NamedPipeServer,
}
#[cfg(unix)]
pub struct Listener {
    socket: tokio::net::UnixListener,
    path: PathBuf,
}
// Explicit protected ACL: only the current token user can access the pipe.
// Default Windows pipe ACLs include Everyone read access; do not inherit them.
#[cfg(windows)]
fn secure_pipe(
    name: &str,
    first: bool,
) -> Result<tokio::net::windows::named_pipe::NamedPipeServer> {
    use windows::{
        core::{PCWSTR, PWSTR},
        Win32::{
            Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL},
            Security::{
                Authorization::{
                    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                },
                GetTokenInformation, TokenUser, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES,
                TOKEN_QUERY, TOKEN_USER,
            },
            System::Threading::{GetCurrentProcess, OpenProcessToken},
        },
    };
    struct Token(HANDLE);
    impl Drop for Token {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
    struct Allocation(*mut std::ffi::c_void);
    impl Drop for Allocation {
        fn drop(&mut self) {
            unsafe {
                let _ = LocalFree(HLOCAL(self.0));
            }
        }
    }
    // All pointers passed to Windows are backed by aligned live storage, and
    // LocalAlloc-owned descriptors remain alive until CreateNamedPipe returns.
    unsafe {
        let mut handle = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut handle)?;
        let token = Token(handle);
        let mut size = 0;
        let _ = GetTokenInformation(token.0, TokenUser, None, 0, &mut size);
        ensure!(
            size as usize >= std::mem::size_of::<TOKEN_USER>(),
            "Cannot determine pipe owner SID"
        );
        let mut storage = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
        GetTokenInformation(
            token.0,
            TokenUser,
            Some(storage.as_mut_ptr().cast()),
            size,
            &mut size,
        )?;
        let user = &*storage.as_ptr().cast::<TOKEN_USER>();
        let mut sid = PWSTR::null();
        ConvertSidToStringSidW(user.User.Sid, &mut sid)?;
        let _sid = Allocation(sid.0.cast());
        let sddl = format!("D:P(A;;GA;;;{})", sid.to_string()?);
        let wide = sddl.encode_utf16().chain(Some(0)).collect::<Vec<_>>();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(wide.as_ptr()),
            1,
            &mut descriptor,
            None,
        )?;
        let _descriptor = Allocation(descriptor.0);
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: false.into(),
        };
        Ok(tokio::net::windows::named_pipe::ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(
                name,
                (&attributes as *const SECURITY_ATTRIBUTES)
                    .cast_mut()
                    .cast(),
            )?)
    }
}

impl Listener {
    pub fn bind(worker: &Worker) -> Result<Self> {
        #[cfg(windows)]
        {
            let pipe = secure_pipe(&worker.endpoint, true)?;
            Ok(Self {
                name: worker.endpoint.clone(),
                pipe,
            })
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = PathBuf::from(&worker.endpoint);
            let socket = tokio::net::UnixListener::bind(&path)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
            Ok(Self { socket, path })
        }
    }
    pub async fn accept(&mut self) -> Result<LocalStream> {
        #[cfg(windows)]
        {
            self.pipe.connect().await?;
            let next = secure_pipe(&self.name, false)?;
            Ok(Box::new(std::mem::replace(&mut self.pipe, next)))
        }
        #[cfg(unix)]
        {
            Ok(Box::new(self.socket.accept().await?.0))
        }
    }
}
#[cfg(unix)]
impl Drop for Listener {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

async fn connect(worker: &Worker) -> Result<LocalStream> {
    #[cfg(windows)]
    {
        Ok(Box::new(
            tokio::net::windows::named_pipe::ClientOptions::new().open(&worker.endpoint)?,
        ))
    }
    #[cfg(unix)]
    {
        Ok(Box::new(
            tokio::net::UnixStream::connect(&worker.endpoint).await?,
        ))
    }
}
pub async fn read_request(stream: LocalStream) -> Result<(LocalStream, Request)> {
    let mut reader = BufReader::new(stream);
    let mut bytes = Vec::new();
    use tokio::io::AsyncReadExt;
    (&mut reader)
        .take(MAX_FRAME as u64 + 1)
        .read_until(b'\n', &mut bytes)
        .await?;
    ensure!(
        bytes.len() <= MAX_FRAME && bytes.last() == Some(&b'\n'),
        "Invalid or oversized worker request"
    );
    let request = serde_json::from_slice(&bytes)?;
    Ok((reader.into_inner(), request))
}
pub async fn reply(mut stream: LocalStream, response: &Reply) -> Result<()> {
    let mut bytes = serde_json::to_vec(response)?;
    bytes.push(b'\n');
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    Ok(())
}
async fn read_reply(stream: LocalStream) -> Result<(LocalStream, Reply)> {
    use tokio::io::AsyncReadExt;
    let mut reader = BufReader::new(stream);
    let mut bytes = Vec::new();
    (&mut reader)
        .take(MAX_FRAME as u64 + 1)
        .read_until(b'\n', &mut bytes)
        .await?;
    ensure!(
        bytes.len() <= MAX_FRAME && bytes.last() == Some(&b'\n'),
        "Invalid worker response"
    );
    let response = serde_json::from_slice(&bytes)?;
    Ok((reader.into_inner(), response))
}
pub async fn call(worker: &Worker, request: &Request) -> Result<Reply> {
    tokio::time::timeout(Duration::from_secs(5), async {
        let mut stream = connect(worker).await?;
        let mut bytes = serde_json::to_vec(request)?;
        ensure!(bytes.len() < MAX_FRAME, "Worker request exceeds 16 MiB");
        bytes.push(b'\n');
        stream.write_all(&bytes).await?;
        stream.flush().await?;
        let (_, response) = read_reply(stream).await?;
        ensure!(
            response.ok,
            "{}",
            response
                .error
                .as_deref()
                .unwrap_or("Worker rejected request")
        );
        Ok(response)
    })
    .await
    .context("Session worker IPC timed out")?
}

pub async fn ensure_worker(
    root: &Path,
    executable: &Path,
    session: &str,
    workspace: &Path,
) -> Result<Worker> {
    let base = worker_path(root, session)?;
    fs::create_dir_all(&base)?;
    // Serialize recovery and startup across frontends. OS releases this lock on exit.
    let lock = fs::OpenOptions::new().create(true).truncate(false).read(true).write(true)
        .open(base.join("startup.lock"))?;
    lock.try_lock().context("Session worker startup or recovery is already in progress; retry shortly")?;
    if let Some(worker) = current(root, session)? {
        ensure!(worker.root == root && worker.session_id == session, "Session worker metadata does not match the requested session");
        if worker.status != "stopped" {
            if jobs::now().saturating_sub(worker.heartbeat_ms) < 10_000 {
                return wait_ready(worker).await;
            }
            recovery::recover(&worker)?;
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    fs::create_dir_all(root.join("background-tasks"))?;
    let mut claim = SessionClaim::acquire(root, session, &id)?;
    let dir = worker_path(root, session)?.join(&id);
    fs::create_dir_all(&dir)?;
    #[cfg(windows)]
    let endpoint = format!(r"\\.\pipe\claude-rust-session-{id}");
    #[cfg(unix)]
    let endpoint = std::env::temp_dir()
        .join(format!("claude-rust-{id}.sock"))
        .to_string_lossy()
        .into_owned();
    let worker = Worker {
        id,
        session_id: session.into(),
        root: root.into(),
        workspace: workspace.into(),
        executable: executable.into(),
        endpoint,
        status: "starting".into(),
        supervisor_pid: None,
        child_pid: None,
        heartbeat_ms: jobs::now(),
        active_job: None,
        error: None,
    };
    jobs::save(&dir.join("worker.json"), &worker)?;
    jobs::save(
        &worker_path(root, session)?.join("current.json"),
        &worker.id,
    )?;
    let mut command = Command::new(executable);
    command
        .env(SUPERVISOR_ENV, &dir)
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
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let mut failed = worker.clone();
            failed.status = "stopped".into();
            failed.error = Some(error.to_string());
            jobs::save(&dir.join("worker.json"), &failed)?;
            return Err(error.into());
        }
    };
    claim.committed = true;
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    wait_ready(worker).await
}
async fn wait_ready(worker: Worker) -> Result<Worker> {
    let dir = directory(&worker);
    tokio::time::timeout(Duration::from_secs(12), async {
        loop {
            let observed = read_worker(&dir)?;
            ensure!(
                observed.status != "stopped",
                "Session worker startup failed: {}",
                observed.error.unwrap_or_default()
            );
            if observed.status == "running"
                && observed.child_pid.is_some()
                && call(&observed, &Request::Ping).await.is_ok()
            {
                return Ok(observed);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .context("Session worker did not become ready within 12 seconds")?
}

pub fn begin_job(worker: &Worker, id: &str, prompt: &str) -> Result<Job> {
    let active = directory(worker).join("active.json");
    ensure!(
        !active.exists(),
        "Session worker is already executing a turn"
    );
    let dir = jobs::directory(&worker.root, id)?;
    ensure!(
        !dir.exists(),
        "Turn ID already exists; it will not be executed twice"
    );
    fs::create_dir_all(&dir)?;
    let job = Job {
        id: id.into(),
        foreground: true,
        parent_session_id: worker.session_id.clone(),
        session_id: worker.session_id.clone(),
        title: prompt.chars().take(160).collect(),
        status: "starting".into(),
        supervisor_pid: read_worker(&directory(worker))?.supervisor_pid,
        child_pid: Some(std::process::id()),
        started_ms: jobs::now(),
        heartbeat_ms: jobs::now(),
        finished_ms: None,
        exit_code: None,
        error: None,
    };
    jobs::save(&dir.join("state.json"), &job)?;
    fs::File::create(dir.join("output.jsonl"))?;
    jobs::save(&active, &id)?;
    Ok(job)
}

/// The durable target lets historical task attachments avoid changing the owner UI state.
pub fn job_execution_session(root: &Path, job: &Job) -> Result<String> {
    let path = jobs::directory(root, &job.id)?.join("owned-child.json");
    match fs::read(path) {
        Ok(bytes) => {
            let id: String = serde_json::from_slice(&bytes)?;
            ensure!(uuid::Uuid::parse_str(&id).is_ok(), "Invalid owned child job target");
            Ok(id)
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(job.session_id.clone()),
        Err(error) => Err(error.into()),
    }
}
pub async fn finish_job(worker: &Worker, id: &str, error: Option<String>) -> Result<()> {
    let dir = jobs::directory(&worker.root, id)?;
    jobs::save(&dir.join("completion.json"), &error)?;
    for _ in 0..100 {
        if jobs::read_job(&dir)?.finished_ms.is_some() {
            fs::remove_file(directory(worker).join("active.json"))?;
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    anyhow::bail!("Supervisor did not acknowledge completed turn")
}
pub async fn shutdown(root: &Path, session: &str) -> Result<()> {
    if let Some(worker) = current(root, session)? {
        if worker.status != "stopped" {
            call(&worker, &Request::Shutdown).await?;
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if read_worker(&directory(&worker))?.status == "stopped" {
                        return Ok::<_, anyhow::Error>(());
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            })
            .await
            .context("Session worker shutdown timed out")??;
        }
    }
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkerRuntimeStatus {
    pub state: String,
    pub pid: Option<u32>,
    pub tools: Option<super::runtime_status::RuntimeStatus>,
}

/// Probe only an existing worker. Never spawns one and supports older binaries
/// by returning a bounded unavailable/incompatible observation.
pub async fn runtime_status(root: &Path, session: &str) -> WorkerRuntimeStatus {
    let mut report = WorkerRuntimeStatus {
        state: "not_started".into(),
        pid: None,
        tools: None,
    };
    let worker = match current(root, session) {
        Ok(Some(worker)) => worker,
        Ok(None) => return report,
        Err(_) => {
            report.state = "metadata_unreadable".into();
            return report;
        }
    };
    if worker.session_id != session || worker.root != root {
        report.state = "metadata_mismatch".into();
        return report;
    }
    report.pid = worker.child_pid;
    if worker.status == "stopped" {
        report.state = "stopped".into();
        return report;
    }
    match tokio::time::timeout(
        Duration::from_millis(1200),
        call(&worker, &Request::RuntimeStatus),
    )
    .await
    {
        Ok(Ok(reply)) => {
            report.state = if reply.runtime_status.is_some() {
                "reachable"
            } else {
                "unsupported_status_protocol"
            }
            .into();
            report.tools = reply.runtime_status;
        }
        Ok(Err(_)) => report.state = "unavailable_or_incompatible".into(),
        Err(_) => report.state = "probe_timeout".into(),
    }
    report
}

pub async fn set_permission_mode(root: &Path, session: &str, job_id: &str, mode: &str) -> Result<()> {
    ensure!(matches!(super::session_permission_mode::parse(mode)?, crate::permissions::PermissionMode::AskPermissions | crate::permissions::PermissionMode::AcceptEdits | crate::permissions::PermissionMode::PlanMode | crate::permissions::PermissionMode::DontAsk | crate::permissions::PermissionMode::Auto), "Live mode switching supports ask, accept-edits, plan, dont-ask and configured auto; bypass requires explicit configuration");
    let worker = current(root, session)?.context("Session has no persistent worker")?;
    let job = jobs::get(root, job_id)?;
    ensure!(job.session_id == session && job.finished_ms.is_none(), "Mode change must target the active job in this session");
    call(&worker, &Request::SetPermissionMode { job_id: job_id.into(), mode: mode.into() }).await?;
    Ok(())
}

pub async fn permission_mode(root: &Path, session: &str, job_id: &str) -> Result<String> {
    Ok(permission_mode_target(root, session, job_id).await?.0)
}

pub async fn permission_mode_target(
    root: &Path,
    session: &str,
    job_id: &str,
) -> Result<(String, String)> {
    let reply = permission_snapshot(root, session, job_id).await?;
    Ok((
        reply
            .permission_mode
            .context("Worker did not return its permission mode")?,
        reply
            .permission_session_id
            .unwrap_or_else(|| session.to_owned()),
    ))
}

/// Read the active executor's authority without changing the frontend's next launch.
pub async fn permission_snapshot(root: &Path, session: &str, job_id: &str) -> Result<Reply> {
    let worker = current(root, session)?.context("Session has no persistent worker")?;
    let job = jobs::get(root, job_id)?;
    ensure!(
        job.session_id == session,
        "Mode query targets another session"
    );
    let reply = call(
        &worker,
        &Request::GetPermissionMode {
            job_id: job_id.into(),
        },
    )
    .await?;
    ensure!(
        reply.job_id.as_deref() == Some(job_id),
        "Permission response targets a different job"
    );
    Ok(reply)
}

pub fn run_supervisor_from_env() -> Result<bool> {
    let Some(value) = std::env::var_os(SUPERVISOR_ENV) else {
        return Ok(false);
    };
    std::env::remove_var(SUPERVISOR_ENV);
    let dir = PathBuf::from(value);
    let mut worker = read_worker(&dir)?;
    let result = supervise(&dir, &mut worker);
    worker.status = "stopped".into();
    worker.heartbeat_ms = jobs::now();
    worker.error = result.err().map(|error| format!("{error:#}"));
    if let (Some(error), Some(id)) = (&worker.error, &worker.active_job) {
        if let Ok(job_dir) = jobs::directory(&worker.root, id) {
            if let Ok(mut job) = jobs::read_job(&job_dir) {
                if job.finished_ms.is_none() {
                    job.status = "failed".into();
                    job.error = Some(error.clone());
                    job.finished_ms = Some(jobs::now());
                    job.heartbeat_ms = jobs::now();
                    job.exit_code = Some(1);
                    let _ = jobs::save(&job_dir.join("state.json"), &job);
                }
            }
        }
    }
    SessionClaim::release(&worker.root, &worker.session_id, &worker.id)?;
    jobs::save(&dir.join("worker.json"), &worker)?;
    Ok(true)
}
fn supervise(dir: &Path, worker: &mut Worker) -> Result<()> {
    let mut command = Command::new(&worker.executable);
    command
        .env(HOST_ENV, dir)
        .env_remove(SUPERVISOR_ENV)
        .current_dir(&worker.workspace)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
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
    let child = command.spawn()?;
    struct Owner(std::process::Child);
    impl Drop for Owner {
        fn drop(&mut self) {
            if self.0.try_wait().ok().flatten().is_none() {
                #[cfg(windows)]
                {
                    use std::os::windows::process::CommandExt;
                    let _ = Command::new("taskkill.exe")
                        .args(["/PID", &self.0.id().to_string(), "/T", "/F"])
                        .creation_flags(0x08000000)
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status();
                }
                #[cfg(unix)]
                {
                    let _ = Command::new("kill")
                        .args(["-TERM", "--", &format!("-{}", self.0.id())])
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status();
                }
            }
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = Owner(child);
    worker.supervisor_pid = Some(std::process::id());
    worker.child_pid = Some(child.0.id());
    worker.status = "running".into();
    let mut cancelling: Option<(String, std::time::Instant)> = None;
    loop {
        let exited = child.0.try_wait()?;
        worker.active_job = fs::read(dir.join("active.json"))
            .ok()
            .map(|bytes| serde_json::from_slice(&bytes))
            .transpose()?;
        if let Some(id) = &worker.active_job {
            let job_dir = jobs::directory(&worker.root, id)?;
            let mut job = jobs::read_job(&job_dir)?;
            if job.finished_ms.is_none() {
                let cancel = job_dir.join("cancel").exists();
                let complete = fs::read(job_dir.join("completion.json")).ok();
                if complete.is_some() || exited.is_some() {
                    let error: Option<String> = complete
                        .map(|bytes| serde_json::from_slice(&bytes))
                        .transpose()?
                        .flatten();
                    // A bare "No" to an approval stops the turn like Esc does.
                    let declined = error
                        .as_deref()
                        .is_some_and(|error| error.contains("Tool permission interrupted the turn"));
                    job.status = if cancel || (declined && exited.is_none()) {
                        "cancelled"
                    } else if exited.is_some() || error.is_some() {
                        "failed"
                    } else {
                        "completed"
                    }
                    .into();
                    job.error = error.or_else(|| {
                        exited.map(|status| format!("Session worker exited unexpectedly: {status}"))
                    });
                    job.exit_code = Some(if job.status == "completed" { 0 } else { 1 });
                    job.finished_ms = Some(jobs::now());
                } else {
                    job.status = "running".into();
                    if cancel {
                        if cancelling
                            .as_ref()
                            .is_none_or(|(previous, _)| previous != id)
                        {
                            cancelling = Some((id.clone(), std::time::Instant::now()));
                        }
                        if cancelling.as_ref().unwrap().1.elapsed() >= Duration::from_secs(2) {
                            #[cfg(windows)]
                            {
                                use std::os::windows::process::CommandExt;
                                let _ = Command::new("taskkill.exe")
                                    .args(["/PID", &child.0.id().to_string(), "/T", "/F"])
                                    .creation_flags(0x08000000)
                                    .stdout(Stdio::null())
                                    .stderr(Stdio::null())
                                    .status();
                            }
                            #[cfg(unix)]
                            {
                                let _ = Command::new("kill")
                                    .args(["-TERM", "--", &format!("-{}", child.0.id())])
                                    .status();
                            }
                            let _ = child.0.kill();
                        }
                    }
                }
                job.heartbeat_ms = jobs::now();
                jobs::save(&job_dir.join("state.json"), &job)?;
            }
        } else {
            cancelling = None;
        }
        worker.heartbeat_ms = jobs::now();
        jobs::save(&dir.join("worker.json"), worker)?;
        if exited.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

static EVENT_SINK: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);
pub fn set_event_sink(path: Option<PathBuf>) {
    *EVENT_SINK.lock().unwrap() = path;
}
pub fn emit_event(value: &serde_json::Value) -> Result<bool> {
    let sink = EVENT_SINK.lock().unwrap();
    let Some(path) = sink.as_ref() else {
        return Ok(false);
    };
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    let mut file = fs::OpenOptions::new().append(true).open(path)?;
    file.write_all(&bytes)?;
    file.flush()?;
    Ok(true)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::{
        Foundation::{CloseHandle, LocalFree, HANDLE, HLOCAL},
        Security::{
            Authorization::{GetSecurityInfo, SE_KERNEL_OBJECT},
            EqualSid, GetAce, GetTokenInformation, TokenUser, ACCESS_ALLOWED_ACE, ACL,
            DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, TOKEN_QUERY, TOKEN_USER,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    #[tokio::test]
    async fn named_pipe_dacl_grants_only_current_token_user() {
        let name = format!(r"\\.\pipe\claude-rust-acl-test-{}", uuid::Uuid::new_v4());
        let pipe = secure_pipe(&name, true).unwrap();
        unsafe {
            let mut descriptor = PSECURITY_DESCRIPTOR::default();
            let mut acl: *mut ACL = std::ptr::null_mut();
            GetSecurityInfo(
                HANDLE(pipe.as_raw_handle()),
                SE_KERNEL_OBJECT,
                DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(&mut acl),
                None,
                Some(&mut descriptor),
            )
            .ok()
            .unwrap();
            assert!(!acl.is_null(), "a null DACL would grant everyone access");
            assert_eq!((*acl).AceCount, 1, "no inherited Everyone or group ACEs");
            let mut ace = std::ptr::null_mut();
            GetAce(acl, 0, &mut ace).unwrap();
            let ace = &*(ace.cast::<ACCESS_ALLOWED_ACE>());
            assert_eq!(ace.Header.AceType, 0, "expected ACCESS_ALLOWED_ACE");
            let mut token = HANDLE::default();
            OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).unwrap();
            let mut size = 0;
            let _ = GetTokenInformation(token, TokenUser, None, 0, &mut size);
            let mut buffer = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
            GetTokenInformation(
                token,
                TokenUser,
                Some(buffer.as_mut_ptr().cast()),
                size,
                &mut size,
            )
            .unwrap();
            let user = &*buffer.as_ptr().cast::<TOKEN_USER>();
            EqualSid(
                PSID((&ace.SidStart as *const u32).cast_mut().cast()),
                user.User.Sid,
            )
            .unwrap();
            CloseHandle(token).unwrap();
            let _ = LocalFree(HLOCAL(descriptor.0));
        }
    }
}

#[cfg(test)]
mod runtime_status_tests {
    use super::*;
    #[tokio::test]
    async fn runtime_status_never_starts_worker_and_bounds_old_or_unresponsive_protocols() {
        let dir = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        assert_eq!(
            runtime_status(dir.path(), &session).await.state,
            "not_started"
        );
        assert!(!dir.path().join("session-workers").exists());
        for behavior in ["old", "unresponsive"] {
            let id = uuid::Uuid::new_v4().to_string();
            let endpoint = if cfg!(windows) {
                format!(r"\\.\pipe\claude-status-{id}")
            } else {
                dir.path()
                    .join(format!("{id}.sock"))
                    .to_string_lossy()
                    .into_owned()
            };
            let worker = Worker {
                id: id.clone(),
                session_id: session.clone(),
                root: dir.path().into(),
                workspace: dir.path().into(),
                executable: std::env::current_exe().unwrap(),
                endpoint,
                status: "running".into(),
                supervisor_pid: None,
                child_pid: None,
                heartbeat_ms: 0,
                active_job: None,
                error: None,
            };
            std::fs::create_dir_all(directory(&worker)).unwrap();
            jobs::save(&directory(&worker).join("worker.json"), &worker).unwrap();
            jobs::save(
                &worker_path(dir.path(), &session)
                    .unwrap()
                    .join("current.json"),
                &id,
            )
            .unwrap();
            let mut listener = Listener::bind(&worker).unwrap();
            let server = tokio::spawn(async move {
                let stream = listener.accept().await.unwrap();
                let (stream, _) = read_request(stream).await.unwrap();
                if behavior == "old" {
                    reply(stream, &Reply::ok(None)).await.unwrap();
                } else {
                    let _stream = stream;
                    std::future::pending::<()>().await;
                }
            });
            let report = runtime_status(dir.path(), &session).await;
            assert_eq!(
                report.state,
                if behavior == "old" {
                    "unsupported_status_protocol"
                } else {
                    "probe_timeout"
                }
            );
            assert!(report.tools.is_none());
            server.abort();
        }
    }
}
