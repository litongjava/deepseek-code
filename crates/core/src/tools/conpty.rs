//! Managed Windows ConPTY, using portable-pty and the existing task/log contract.
//!
//! Input is routed only inside the original owner process. The registry key is
//! canonical root + session + task, after the normal durable ownership check;
//! there is no unauthenticated IPC endpoint or PID-based input fallback.
//!
//! portable-pty cannot create a suspended child. Instead it starts our host's
//! early execution gate. Before spawning ANY user program the gate joins an
//! owner-created, non-breakaway, kill-on-close Job Object and closes its own job
//! handle. If the owner died, opening the job fails or closing the last handle
//! kills the gate. Thus a command never runs outside the job, even in the spawn
//! / assignment window. The job also covers grandchildren and owner hard exit.
use anyhow::{bail, ensure, Context, Result};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    ffi::OsString,
    io::{Read, Write},
    os::windows::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex, OnceLock, Weak,
    },
    time::{Duration, Instant},
};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::{JobObjects::*, Threading::*, IO::CancelSynchronousIo},
    },
};

const GATE: &str = "--dcl-internal-conpty-gate";
const TICK: Duration = Duration::from_millis(5);
const CLEANUP_BOUND: Duration = Duration::from_secs(10);

type Key = (PathBuf, String, String);
type Registry = Mutex<HashMap<Key, Weak<Control>>>;
static OWNERS: OnceLock<Registry> = OnceLock::new();
fn registry() -> &'static Registry {
    OWNERS.get_or_init(Default::default)
}
fn key(root: &Path, session: &str, id: &str) -> Result<Key> {
    Ok((
        root.canonicalize()?,
        uuid::Uuid::parse_str(session)?.to_string(),
        uuid::Uuid::parse_str(id)?.to_string(),
    ))
}

pub(super) struct SpawnSpec {
    pub executable: OsString,
    pub arguments: Vec<OsString>,
    pub cwd: PathBuf,
    pub environment: Vec<(OsString, OsString)>,
    pub environment_overrides: Vec<(OsString, Option<OsString>)>,
    pub rows: u16,
    pub cols: u16,
}

struct Handle(HANDLE);
// Owned, non-inheritable kernel handles; OS operations are thread safe. No
// borrowed/pseudo handle is ever put in this wrapper.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
struct Job {
    handle: Handle,
    name: OsString,
}
impl Job {
    fn new() -> Result<Self> {
        let name: OsString = format!("Local\\dcl-conpty-{}", uuid::Uuid::new_v4()).into();
        let wide: Vec<u16> = name.encode_wide().chain(Some(0)).collect();
        let job = Self {
            handle: Handle(unsafe { CreateJobObjectW(None, PCWSTR(wide.as_ptr()))? }),
            name,
        };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        unsafe {
            SetInformationJobObject(
                job.handle.0,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as _,
                std::mem::size_of_val(&limits) as u32,
            )?;
        }
        Ok(job)
    }
    fn terminate(&self) -> Result<()> {
        unsafe {
            TerminateJobObject(self.handle.0, 1)?;
        }
        Ok(())
    }
    fn active(&self) -> Result<u32> {
        let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        unsafe {
            QueryInformationJobObject(
                self.handle.0,
                JobObjectBasicAccountingInformation,
                &mut info as *mut _ as _,
                std::mem::size_of_val(&info) as u32,
                None,
            )?;
        }
        Ok(info.ActiveProcesses)
    }
    fn stop(&self) -> Result<()> {
        self.terminate()?;
        let deadline = Instant::now() + CLEANUP_BOUND;
        while self.active()? != 0 {
            ensure!(
                Instant::now() < deadline,
                "ConPTY job still contains live processes after termination"
            );
            std::thread::sleep(TICK);
        }
        Ok(())
    }
}

/// Runs before CLI/Desktop runtime or argument parsing, in a ConPTY-attached
/// copy of the host. The actual program/arguments/environment/cwd come from the
/// ordinary Bash builder. No shell text is reconstructed or stored in a file.
pub(super) fn run_gate() -> Result<Option<i32>> {
    let mut args = std::env::args_os().skip(1);
    if args.next().as_deref() != Some(std::ffi::OsStr::new(GATE)) {
        return Ok(None);
    }
    let job_name = args.next().context("ConPTY gate missing job")?;
    let cwd = args.next().context("ConPTY gate missing cwd")?;
    let executable = args.next().context("ConPTY gate missing program")?;
    let wide: Vec<u16> = job_name.encode_wide().chain(Some(0)).collect();
    // JOB_OBJECT_ASSIGN_PROCESS (0x1), deliberately no terminate/query access.
    let job = Handle(unsafe { OpenJobObjectW(1, false, PCWSTR(wide.as_ptr()))? });
    unsafe {
        AssignProcessToJobObject(job.0, GetCurrentProcess())?;
    }
    // This MUST precede user spawn: never keep the kill-on-close job alive on
    // behalf of a dead owner. Children inherit job membership, not its handle.
    drop(job);
    let status = std::process::Command::new(executable)
        .args(args)
        // portable-pty may fall back to HOME when a requested cwd disappears.
        // Reapply the actual Bash cwd here so the user program fails to spawn
        // instead of ever running in a substituted directory.
        .current_dir(cwd)
        .status()
        .context("ConPTY gate could not start the configured shell")?;
    // Preserve the full Windows exit code (the host ExitCode adapter is u8).
    std::process::exit(status.code().unwrap_or(1))
}

fn gate_command(spec: &SpawnSpec, job_name: &std::ffi::OsStr, host: &Path) -> CommandBuilder {
    let mut command = CommandBuilder::new(host);
    command.arg(GATE);
    command.arg(job_name);
    command.arg(&spec.cwd);
    command.arg(&spec.executable);
    command.args(&spec.arguments);
    command.cwd(&spec.cwd);
    command.env_clear();
    for (name, value) in &spec.environment {
        command.env(name, value);
    }
    for (name, value) in &spec.environment_overrides {
        command.env_remove(name);
        if let Some(value) = value {
            command.env(name, value);
        }
    }
    command
}

struct Active;
impl Active {
    fn new() -> Self {
        super::background_shell::enter_owner();
        Self
    }
}
impl Drop for Active {
    fn drop(&mut self) {
        super::background_shell::leave_owner();
    }
}
struct Control {
    job: Job,
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    writer: Mutex<Option<Box<dyn Write + Send>>>,
    operation: Mutex<()>,
    stopping: AtomicBool,
    finished: AtomicBool,
}
pub(super) struct OwnerGuard(Arc<Control>);
impl OwnerGuard {
    pub(super) fn is_finished(&self) -> bool {
        self.0.finished.load(Ordering::Acquire)
    }
}
impl Drop for OwnerGuard {
    fn drop(&mut self) {
        if self.is_finished() {
            return;
        }
        self.0.stopping.store(true, Ordering::Release);
        let _ = self.0.job.terminate();
        let deadline = Instant::now() + CLEANUP_BOUND;
        while !self.is_finished() && Instant::now() < deadline {
            std::thread::sleep(TICK);
        }
    }
}

fn update(root: &Path, session: &str, id: &str, status: &str, output: Value) -> Result<()> {
    super::tasks::execute_for_shell_owner(
        "TaskUpdate",
        json!({"id":id,"status":status,"output":output.to_string()}),
        root,
        session,
    )?;
    Ok(())
}

pub(super) fn start(
    spec: SpawnSpec,
    root: &Path,
    session: &str,
    description: &str,
    command_text: &str,
) -> Result<(Value, OwnerGuard)> {
    ensure!(
        (1..=500).contains(&spec.rows) && (1..=1000).contains(&spec.cols),
        "Invalid ConPTY dimensions"
    );
    let session = uuid::Uuid::parse_str(session)?.to_string();
    let task = super::tasks::execute_for_shell_owner(
        "TaskCreate",
        json!({"subject":description,
        "metadata":{"managed_shell":true,"interaction":"pty","backend":"conpty",
        "merged_output":true,"input_scope":"owner_process","command":command_text,
        "started_ms":chrono::Utc::now().timestamp_millis()}}),
        root,
        &session,
    )?;
    let id = task["id"].as_str().context("Missing task ID")?.to_owned();
    let directory = root.join("shell-tasks").join(&session).join(&id);
    let active = Active::new();
    let prepared = (|| -> Result<_> {
        let key = key(root, &session, &id)?;
        std::fs::create_dir_all(&directory)?;
        let output = std::fs::File::create(directory.join("stdout.log"))?;
        std::fs::File::create(directory.join("stderr.log"))?;
        std::fs::write(directory.join("owner.pid"), std::process::id().to_string())?;
        std::fs::write(directory.join("heartbeat"), b"running")?;
        let job = Job::new()?;
        let command = gate_command(&spec, &job.name, &std::env::current_exe()?);
        let pair = native_pty_system().openpty(PtySize {
            rows: spec.rows,
            cols: spec.cols,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        let control = Arc::new(Control {
            job,
            master: Mutex::new(Some(pair.master)),
            writer: Mutex::new(Some(writer)),
            operation: Mutex::new(()),
            stopping: AtomicBool::new(false),
            finished: AtomicBool::new(false),
        });
        // portable-pty requests inherited cursor state. A response worker keeps
        // output draining independent of synchronous input/ClosePseudoConsole.
        let reader_owner = Arc::downgrade(&control);
        let reader = std::thread::Builder::new()
            .name("conpty-output".into())
            .spawn(move || capture(reader, output, reader_owner))?;
        // Every fallible step after spawn must terminate the job before closing
        // the console. Resources below handle both startup and thread failure.
        let child = pair.slave.spawn_command(command);
        drop(pair.slave);
        let mut owned = Owned {
            control: control.clone(),
            child: None,
            reader: Some(reader),
        };
        owned.child = Some(child?);
        let pid = owned
            .child
            .as_ref()
            .unwrap()
            .process_id()
            .context("ConPTY gate has no PID")?;
        update(root, &session, &id, "running", json!({"terminal_pid":pid}))?;
        registry()
            .lock()
            .unwrap()
            .insert(key.clone(), Arc::downgrade(&control));
        let root = root.to_owned();
        let session = session.clone();
        let owner_id = id.clone();
        let directory = directory.clone();
        std::thread::Builder::new()
            .name("conpty-owner".into())
            .spawn(move || {
                let _active = active;
                let mut outcome = owned.run(&directory);
                if let Err(error) =
                    std::fs::write(directory.join("terminal-state.json"), outcome.to_string())
                {
                    outcome["status"] = json!("failed");
                    outcome["is_error"] = json!(true);
                    outcome["terminal_record_error"] = json!(error.to_string());
                    tracing::error!(%error, "ConPTY terminal state could not be written");
                }
                let status = outcome["status"].as_str().unwrap_or("failed");
                if let Err(error) = update(&root, &session, &owner_id, status, outcome.clone()) {
                    tracing::error!(%error, "ConPTY terminal task could not be updated");
                }
                registry().lock().unwrap().remove(&key);
                owned.control.finished.store(true, Ordering::Release);
            })?;
        Ok((
            json!({"task_id":id,"status":"running","terminal_pid":pid,"interaction":"pty",
            "backend":"conpty","merged_output":true,"rows":spec.rows,"cols":spec.cols,
            "message":"ConPTY started. TaskInput requires this owner process; eof:true is unsupported. Use application-specific data or TaskStop."}),
            OwnerGuard(control),
        ))
    })();
    match prepared {
        Ok(value) => Ok(value),
        Err(error) => {
            let facts =
                json!({"is_error":true,"error":format!("{error:#}"),"initialization_failed":true});
            update(root, &session, &id, "failed", facts).with_context(|| {
                format!("ConPTY initialization failed ({error:#}); could not finalize task {id}")
            })?;
            Err(error.context(format!("ConPTY initialization failed for task {id}")))
        }
    }
}

// Keep draining after a log write failure, otherwise closing the pseudoconsole
// can deadlock. A capture failure still makes the final task failed.
fn capture(
    mut reader: Box<dyn Read + Send>,
    mut output: std::fs::File,
    owner: Weak<Control>,
) -> Result<()> {
    let (query_tx, query_rx) = mpsc::sync_channel::<()>(1);
    let response = std::thread::Builder::new()
        .name("conpty-cursor".into())
        .spawn(move || -> Result<()> {
            while query_rx.recv().is_ok() {
                let Some(owner) = owner.upgrade() else {
                    break;
                };
                let deadline = Instant::now() + Duration::from_secs(2);
                // The input mutex serializes the actual pipe writes. These protocol
                // replies remain permitted during close, when ConPTY can request
                // cursor state again; they are never model data or an EOF.
                let (_, error) =
                    write_cancellable(&owner, b"\x1b[1;1R", || Instant::now() >= deadline)?;
                if let Some(error) = error {
                    bail!("ConPTY cursor response: {error}");
                }
            }
            Ok(())
        })?;
    let mut failure = None;
    let mut bytes = [0u8; 8192];
    let mut query = CursorQuery::default();
    loop {
        match reader.read(&mut bytes) {
            Ok(0) => break,
            Ok(n) => {
                // Requests may span reads. Preserve every raw output byte in
                // the log; only recognize DSR, not arbitrary escape commands.
                for byte in &bytes[..n] {
                    if query.feed(*byte) {
                        let _ = query_tx.try_send(());
                    }
                }
                if failure.is_none() {
                    if let Err(e) = output.write_all(&bytes[..n]) {
                        failure = Some(e);
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => break,
            Err(e) => {
                failure.get_or_insert(e);
                break;
            }
        }
    }
    drop(query_tx);
    response
        .join()
        .map_err(|_| anyhow::anyhow!("ConPTY cursor responder panicked"))??;
    if let Some(e) = failure {
        return Err(e.into());
    }
    output.flush()?;
    Ok(())
}

#[derive(Default)]
struct CursorQuery(usize);
impl CursorQuery {
    fn feed(&mut self, byte: u8) -> bool {
        const QUERY: &[u8] = b"\x1b[6n";
        if byte == QUERY[self.0] {
            self.0 += 1;
        } else {
            self.0 = usize::from(byte == QUERY[0]);
        }
        if self.0 == QUERY.len() {
            self.0 = 0;
            true
        } else {
            false
        }
    }
}

// WinChild::try_wait in portable-pty 0.9 tests GetExitCodeProcess against
// STILL_ACTIVE (259), which is also a valid real exit code. Pin the process
// handle and test its signalled state first; WinChild::wait then returns 259
// correctly instead of leaving this task running forever.
fn completed_child(
    child: &mut (dyn Child + Send + Sync),
) -> Result<Option<portable_pty::ExitStatus>> {
    let handle = HANDLE(
        child
            .as_raw_handle()
            .context("ConPTY child has no process handle")?,
    );
    match unsafe { WaitForSingleObject(handle, 0) } {
        WAIT_OBJECT_0 => Ok(Some(child.wait()?)),
        WAIT_TIMEOUT => Ok(None),
        _ => Err(std::io::Error::last_os_error()).context("Waiting for ConPTY process handle"),
    }
}

struct Owned {
    control: Arc<Control>,
    child: Option<Box<dyn Child + Send + Sync>>,
    reader: Option<std::thread::JoinHandle<Result<()>>>,
}
impl Owned {
    fn cleanup(&mut self) -> Result<()> {
        self.control.stopping.store(true, Ordering::Release);
        let mut errors = Vec::new();
        if let Err(e) = self.control.job.terminate() {
            errors.push(format!("initial job termination: {e:#}"));
        }
        if let Some(mut child) = self.child.take() {
            // A gate which failed before joining is not in the job. Its pinned
            // process handle, not a reused PID, is the only fallback target.
            let result = (|| -> Result<()> {
                if completed_child(child.as_mut())?.is_none() {
                    child.kill()?;
                }
                child.wait()?;
                Ok(())
            })();
            if let Err(e) = result {
                errors.push(format!("gate: {e:#}"));
            }
        }
        // The gate may still have been before AssignProcessToJobObject when
        // the first terminate saw an empty job. Pin/kill/reap that gate first,
        // THEN terminate and observe zero members, closing the late-join race.
        if let Err(e) = self.control.job.stop() {
            errors.push(format!("job: {e:#}"));
        }
        // ClosePseudoConsole may request cursor state while closing; retain
        // its input writer until the draining/responder threads have answered.
        self.control.master.lock().unwrap().take();
        self.control.writer.lock().unwrap().take();
        ensure!(errors.is_empty(), "ConPTY cleanup: {}", errors.join("; "));
        Ok(())
    }

    fn run(&mut self, directory: &Path) -> Value {
        let mut cancelled = false;
        let mut error = None;
        let mut code = None;
        loop {
            if self.control.stopping.load(Ordering::Acquire) || directory.join("cancel").exists() {
                cancelled = true;
                break;
            }
            match completed_child(self.child.as_mut().unwrap().as_mut()) {
                Ok(Some(status)) => {
                    code = Some(status.exit_code());
                    break;
                }
                Ok(None) => (),
                Err(e) => {
                    error = Some(e.to_string());
                    break;
                }
            }
            if let Err(e) = std::fs::write(directory.join("heartbeat"), b"running") {
                error = Some(e.to_string());
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let cleanup = self.cleanup();
        let capture = self
            .reader
            .take()
            .map(|reader| {
                reader
                    .join()
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("ConPTY reader panicked")))
            })
            .unwrap_or(Ok(()));
        let status = if cleanup.is_err() || capture.is_err() || error.is_some() {
            "failed"
        } else if cancelled {
            "cancelled"
        } else if code == Some(0) {
            "completed"
        } else {
            "failed"
        };
        json!({"status":status,"is_error":status!="completed","exit_code":if cancelled {None} else {code},
            "cancelled":cancelled,"cleanup_complete":cleanup.is_ok(),"final_drain_complete":capture.is_ok(),
            "cleanup_error":cleanup.err().map(|e|format!("{e:#}")),"output_error":capture.err().map(|e|format!("{e:#}")),"error":error})
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        let _ = self.cleanup();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

pub(super) fn execute(
    input: &Value,
    root: &Path,
    session: &str,
    cancelled: Option<&AtomicBool>,
) -> Result<Value> {
    // shell_input has already validated ALL keys, data, timeout and dimensions,
    // and looked up the real task in this session. This capability check must
    // still precede data; no partial write for data+unsupported EOF.
    ensure!(input["eof"] != true, "Windows ConPTY eof:true is unsupported: there is no universal terminal EOF. Ctrl+Z/Ctrl+D depend on the application; send them explicitly with data, or use TaskStop. No data was written.");
    let id = input["task_id"]
        .as_str()
        .or_else(|| input["taskId"].as_str())
        .or_else(|| input["id"].as_str())
        .context("task_id required")?;
    let key = key(root, session, id)?;
    let owner = registry().lock().unwrap().get(&key).and_then(Weak::upgrade)
        .context("ConPTY input is unavailable outside its original owner process; cross-process TaskInput is unsupported. Resume through the owning persistent worker.")?;
    let deadline =
        Instant::now() + Duration::from_millis(input["timeout"].as_u64().unwrap_or(10_000));
    let interrupted = || {
        owner.stopping.load(Ordering::Acquire)
            || cancelled.is_some_and(|flag| flag.load(Ordering::Acquire))
            || Instant::now() >= deadline
    };
    let _operation = loop {
        ensure!(
            !interrupted(),
            "ConPTY input cancelled, stopped or timed out before writing"
        );
        match owner.operation.try_lock() {
            Ok(lock) => break lock,
            Err(std::sync::TryLockError::WouldBlock) => std::thread::sleep(TICK),
            Err(_) => bail!("ConPTY input lock is poisoned"),
        }
    };
    ensure!(
        !interrupted(),
        "ConPTY input cancelled, stopped or timed out before writing"
    );
    if let Some(size) = input.get("resize") {
        owner
            .master
            .lock()
            .unwrap()
            .as_ref()
            .context("ConPTY is closed")?
            .resize(PtySize {
                rows: size["rows"].as_u64().unwrap() as u16,
                cols: size["cols"].as_u64().unwrap() as u16,
                pixel_width: 0,
                pixel_height: 0,
            })?;
        return Ok(
            json!({"task_id":id,"rows":size["rows"],"cols":size["cols"],"resized":true,"bytes_written":0}),
        );
    }
    let data = input["data"]
        .as_str()
        .context("ConPTY input data required")?
        .as_bytes();
    let (count, error) = write_cancellable(&owner, data, interrupted)?;
    Ok(
        json!({"task_id":id,"bytes_written":count,"eof_sent":false,"is_error":error.is_some(),"error":error,
        "bytes_written_lower_bound":error.is_some(),
        "outcome":if error.is_some() {"unknown"} else {"written"}}),
    )
}

/// portable-pty exposes a synchronous anonymous-pipe writer on Windows. Run
/// one write thread under the per-task operation lock, interrupt its exact OS
/// thread with CancelSynchronousIo, and JOIN before returning. No detached write
/// can resume after timeout/cancellation. Repeated cancellation covers the
/// check-to-WriteFile race; accepted bytes are reported and never replayed.
fn write_cancellable(
    owner: &Control,
    data: &[u8],
    interrupted: impl Fn() -> bool,
) -> Result<(usize, Option<String>)> {
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| -> Result<_> {
        let (thread_tx, thread_rx) = mpsc::sync_channel(1);
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let stop_ref = &stop;
        let writer = scope.spawn(move || {
            let handle =
                unsafe { OpenThread(THREAD_TERMINATE, false, GetCurrentThreadId()) }.map(Handle);
            let handle = match handle {
                Ok(handle) => handle,
                Err(e) => {
                    let _ = thread_tx.send(Err(e.to_string()));
                    return;
                }
            };
            if thread_tx.send(Ok(handle)).is_err() {
                return;
            }
            let mut count = 0;
            let result = (|| -> Result<()> {
                let mut lock = loop {
                    ensure!(
                        !stop_ref.load(Ordering::Acquire),
                        "ConPTY input interrupted before acquiring writer"
                    );
                    match owner.writer.try_lock() {
                        Ok(lock) => break lock,
                        Err(std::sync::TryLockError::WouldBlock) => std::thread::sleep(TICK),
                        Err(_) => bail!("ConPTY writer lock is poisoned"),
                    }
                };
                let writer = lock.as_mut().context("ConPTY input is closed")?;
                while count < data.len() {
                    ensure!(
                        !stop_ref.load(Ordering::Acquire)
                            && !owner.stopping.load(Ordering::Acquire),
                        "ConPTY input interrupted"
                    );
                    let end = (count + 4096).min(data.len());
                    let n = writer.write(&data[count..end])?;
                    ensure!(n != 0, "ConPTY input accepted zero bytes");
                    count += n;
                }
                Ok(())
            })();
            let _ = done_tx.send((count, result.err().map(|e| format!("{e:#}"))));
        });
        let handle = thread_rx.recv()?.map_err(anyhow::Error::msg)?;
        let result = loop {
            if interrupted() {
                stop.store(true, Ordering::Release);
                unsafe {
                    let _ = CancelSynchronousIo(handle.0);
                }
            }
            match done_rx.recv_timeout(TICK) {
                Ok(result) => break result,
                Err(mpsc::RecvTimeoutError::Timeout) => (),
                Err(e) => return Err(e.into()),
            }
        };
        writer
            .join()
            .map_err(|_| anyhow::anyhow!("ConPTY input writer panicked"))?;
        Ok(result)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::io::AsRawHandle;

    #[test]
    fn gate_preserves_argv_cwd_and_case_insensitive_environment_removals() {
        let spec = SpawnSpec {
            executable: "configured shell.exe".into(),
            arguments: vec!["-Command".into(), "literal spaces & unicode 中文".into()],
            cwd: PathBuf::from(r"C:\fixture workspace"),
            environment: vec![
                ("Path".into(), "old path".into()),
                ("Fixture".into(), "old".into()),
            ],
            environment_overrides: vec![
                ("PATH".into(), None),
                ("FIXTURE".into(), Some("new".into())),
            ],
            rows: 24,
            cols: 80,
        };
        let command = gate_command(
            &spec,
            std::ffi::OsStr::new("job"),
            Path::new(r"C:\host.exe"),
        );
        assert!(command.get_env("Path").is_none());
        assert_eq!(
            command.get_env("fixture"),
            Some(std::ffi::OsStr::new("new"))
        );
        assert_eq!(command.get_cwd(), Some(&spec.cwd.as_os_str().to_owned()));
        assert_eq!(
            &command.get_argv()[4..],
            &[vec![spec.executable.clone()], spec.arguments.clone()].concat()
        );
    }

    #[test]
    fn cursor_query_is_recognized_across_read_boundaries_without_matching_other_vt() {
        let mut parser = CursorQuery::default();
        let mut replies = 0;
        for chunk in [b"hello\x1b[".as_slice(), b"6n", b"\x1b[31m", b"\x1b\x1b[6n"] {
            for byte in chunk {
                replies += usize::from(parser.feed(*byte));
            }
        }
        assert_eq!(replies, 2);
    }

    #[test]
    fn eof_rejects_combined_data_before_lookup_or_write() {
        let root = tempfile::tempdir().unwrap();
        let error = execute(
            &json!({"task_id":uuid::Uuid::new_v4().to_string(),
            "data":"must not be written", "eof":true}),
            root.path(),
            &uuid::Uuid::new_v4().to_string(),
            None,
        )
        .unwrap_err();
        assert!(error.to_string().contains("No data was written"));
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_durable_id_without_the_original_process_is_not_an_input_capability() {
        let root = tempfile::tempdir().unwrap();
        let error = execute(
            &json!({"task_id":uuid::Uuid::new_v4().to_string(),"data":"x"}),
            root.path(),
            &uuid::Uuid::new_v4().to_string(),
            None,
        )
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("cross-process TaskInput is unsupported"));
    }

    // Separate-process fixture: runs no child in an ordinary suite invocation.
    #[test]
    fn job_member_fixture() {
        let Some(name) = std::env::var_os("DCL_TEST_CONPTY_JOB") else {
            return;
        };
        let wide: Vec<u16> = name.encode_wide().chain(Some(0)).collect();
        let job = Handle(unsafe { OpenJobObjectW(1, false, PCWSTR(wide.as_ptr())).unwrap() });
        unsafe {
            AssignProcessToJobObject(job.0, GetCurrentProcess()).unwrap();
        }
        drop(job);
        let mut child = std::process::Command::new("powershell.exe")
            .args(["-NoProfile", "-Command", "Start-Sleep -Seconds 600"])
            .spawn()
            .unwrap();
        std::fs::write(
            std::env::var_os("DCL_TEST_CONPTY_PID").unwrap(),
            child.id().to_string(),
        )
        .unwrap();
        let _ = child.wait();
    }

    fn tree() -> (Job, std::process::Child, Handle, tempfile::TempDir) {
        let temp = tempfile::tempdir().unwrap();
        let job = Job::new().unwrap();
        let file = temp.path().join("child.pid");
        let mut gate = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tools::conpty::tests::job_member_fixture",
                "--nocapture",
            ])
            .env("DCL_TEST_CONPTY_JOB", &job.name)
            .env("DCL_TEST_CONPTY_PID", &file)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Ok(text) = std::fs::read_to_string(&file) {
                if let Ok(pid) = text.parse::<u32>() {
                    let child =
                        Handle(unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid).unwrap() });
                    assert_eq!(job.active().unwrap(), 2);
                    println!("CONPTY_JOB gate={} descendant={pid}", gate.id());
                    return (job, gate, child, temp);
                }
            }
            assert!(
                gate.try_wait().unwrap().is_none(),
                "gate exited before publishing child"
            );
            assert!(Instant::now() < deadline, "job fixture timed out");
            std::thread::sleep(TICK);
        }
    }

    #[test]
    fn terminate_job_confirms_the_entire_process_tree_exited() {
        let (job, mut gate, child, _temp) = tree();
        job.stop().unwrap();
        unsafe {
            assert_eq!(
                WaitForSingleObject(HANDLE(gate.as_raw_handle()), 10_000),
                windows::Win32::Foundation::WAIT_OBJECT_0
            );
            assert_eq!(
                WaitForSingleObject(child.0, 10_000),
                windows::Win32::Foundation::WAIT_OBJECT_0
            );
        }
        gate.wait().unwrap();
        assert_eq!(job.active().unwrap(), 0);
    }

    #[test]
    fn closing_the_owner_job_handle_kills_the_entire_process_tree() {
        let (job, mut gate, child, _temp) = tree();
        drop(job);
        unsafe {
            assert_eq!(
                WaitForSingleObject(HANDLE(gate.as_raw_handle()), 10_000),
                windows::Win32::Foundation::WAIT_OBJECT_0
            );
            assert_eq!(
                WaitForSingleObject(child.0, 10_000),
                windows::Win32::Foundation::WAIT_OBJECT_0
            );
        }
        gate.wait().unwrap();
    }

    fn blocked_writer() -> (Control, std::fs::File) {
        use std::os::windows::io::FromRawHandle;
        // Match portable-pty/filedescriptor's SYNCHRONOUS CreatePipe writer.
        // ChildStdin is not a suitable fixture: std may use overlapped named
        // pipes and a different capacity, neither covered by CancelSynchronousIo.
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn CreatePipe(
                read: *mut *mut std::ffi::c_void,
                write: *mut *mut std::ffi::c_void,
                attributes: *const std::ffi::c_void,
                size: u32,
            ) -> i32;
        }
        let mut read = std::ptr::null_mut();
        let mut write = std::ptr::null_mut();
        assert_ne!(
            unsafe { CreatePipe(&mut read, &mut write, std::ptr::null(), 4096) },
            0
        );
        let reader = unsafe { std::fs::File::from_raw_handle(read) };
        let writer = unsafe { std::fs::File::from_raw_handle(write) };
        (
            Control {
                job: Job::new().unwrap(),
                master: Mutex::new(None),
                writer: Mutex::new(Some(Box::new(writer))),
                operation: Mutex::new(()),
                stopping: AtomicBool::new(false),
                finished: AtomicBool::new(false),
            },
            reader,
        )
    }

    #[test]
    fn a_blocked_write_times_out_and_joins_before_returning() {
        let (owner, mut reader) = blocked_writer();
        let data = vec![b'x'; 65536];
        let deadline = Instant::now() + Duration::from_millis(150);
        let (count, error) =
            write_cancellable(&owner, &data, || Instant::now() >= deadline).unwrap();
        assert!(Instant::now() < deadline + Duration::from_secs(5));
        assert!(
            count > 0 && count < data.len(),
            "the writer must make progress before blocking"
        );
        assert!(error.is_some());
        owner.writer.lock().unwrap().take();
        let mut received = Vec::new();
        reader.read_to_end(&mut received).unwrap();
        assert!(received.len() >= count && received.len() < data.len());
    }

    #[test]
    fn cancellation_interrupts_the_blocked_os_thread_without_a_detached_writer() {
        let (owner, mut reader) = blocked_writer();
        let data = vec![b'x'; 65536];
        let flag = Arc::new(AtomicBool::new(false));
        let signal = flag.clone();
        let cancel = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            signal.store(true, Ordering::Release);
        });
        let start = Instant::now();
        let (count, error) =
            write_cancellable(&owner, &data, || flag.load(Ordering::Acquire)).unwrap();
        cancel.join().unwrap();
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(
            count > 0 && count < data.len(),
            "the writer must make progress before blocking"
        );
        assert!(error.is_some());
        owner.writer.lock().unwrap().take();
        let mut received = Vec::new();
        reader.read_to_end(&mut received).unwrap();
        assert!(received.len() >= count && received.len() < data.len());
    }

    #[test]
    fn a_gate_cannot_join_a_job_after_its_owner_closed_it() {
        let job = Job::new().unwrap();
        let wide: Vec<u16> = job.name.encode_wide().chain(Some(0)).collect();
        drop(job);
        assert!(unsafe { OpenJobObjectW(1, false, PCWSTR(wide.as_ptr())) }.is_err());
    }
}
