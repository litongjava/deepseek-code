use anyhow::Result;
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use tokio::process::Command;
use tokio::sync::broadcast;

const REPLAY_BYTES: usize = 256 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum TerminalError {
    #[error("Terminal session not found")]
    NotFound,
    #[error("Terminal has exited")]
    Exited,
    #[error("{0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PtySession {
    pub id: String,
    pub cwd: String,
    pub pid: u32,
    pub shell: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PtyOutput {
    pub session_id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub data: String,
    pub code: Option<u32>,
    pub is_stderr: bool,
}

struct OutputState {
    tx: broadcast::Sender<PtyOutput>,
    replay: VecDeque<PtyOutput>,
    bytes: usize,
    finished: bool,
}

impl OutputState {
    fn publish(&mut self, output: PtyOutput) {
        if self.finished {
            return;
        }
        self.finished = output.kind == "exit";
        self.bytes += output.data.len();
        self.replay.push_back(output.clone());
        while self.bytes > REPLAY_BYTES || self.replay.len() > 2048 {
            if let Some(old) = self.replay.pop_front() {
                self.bytes -= old.data.len();
            }
        }
        let _ = self.tx.send(output);
    }
}

pub struct PtySubscription {
    pub replay: Vec<PtyOutput>,
    pub receiver: broadcast::Receiver<PtyOutput>,
}

type PtyChild = Arc<Mutex<Box<dyn Child + Send + Sync>>>;

struct ManagedSession {
    info: PtySession,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    child: PtyChild,
    output: Arc<Mutex<OutputState>>,
    #[cfg(windows)]
    job: TerminalJob,
}

impl ManagedSession {
    fn stop(&self) -> Result<()> {
        let mut child = self.child.lock().unwrap();
        #[cfg(windows)]
        {
            self.job.terminate()?;
            child.wait()?;
        }
        #[cfg(not(windows))]
        if child.try_wait()?.is_none() {
            #[cfg(unix)]
            {
                use rustix::process::{kill_process_group, Pid, Signal};
                // Only signal groups belonging to this still-owned PTY, before reaping its leader.
                if let Some(group) = self.master.lock().unwrap().process_group_leader() {
                    if let Some(pid) = Pid::from_raw(group) {
                        let _ = kill_process_group(pid, Signal::KILL);
                    }
                }
                if let Some(pid) = Pid::from_raw(self.info.pid as i32) {
                    let _ = kill_process_group(pid, Signal::KILL);
                }
            }
            child.kill()?;
            child.wait()?;
        }
        Ok(())
    }
}

impl Drop for ManagedSession {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            tracing::warn!(%error, "Failed to close owned terminal");
        }
    }
}

#[cfg(windows)]
struct TerminalJob(windows::Win32::Foundation::HANDLE);
#[cfg(windows)]
unsafe impl Send for TerminalJob {}
#[cfg(windows)]
unsafe impl Sync for TerminalJob {}

#[cfg(windows)]
impl TerminalJob {
    fn new(child: &dyn Child) -> Result<Self> {
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::JobObjects::*;
        unsafe {
            let job = Self(CreateJobObjectW(None, None)?);
            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const _,
                std::mem::size_of_val(&limits) as u32,
            )?;
            let handle = child
                .as_raw_handle()
                .ok_or_else(|| anyhow::anyhow!("Missing terminal process handle"))?;
            AssignProcessToJobObject(job.0, HANDLE(handle))?;
            Ok(job)
        }
    }

    fn terminate(&self) -> Result<()> {
        unsafe {
            windows::Win32::System::JobObjects::TerminateJobObject(self.0, 1)?;
        }
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for TerminalJob {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[derive(Clone)]
pub struct PtyManager {
    sessions: Arc<Mutex<HashMap<String, Arc<ManagedSession>>>>,
}

fn output(id: &str, kind: &str, data: String, code: Option<u32>) -> PtyOutput {
    PtyOutput {
        session_id: id.into(),
        kind: kind.into(),
        data,
        code,
        is_stderr: false,
    }
}

// Preserve incomplete UTF-8 sequences between reads instead of replacing split characters.
fn decode_utf8(pending: &mut Vec<u8>, eof: bool) -> String {
    let mut text = String::new();
    loop {
        match std::str::from_utf8(pending) {
            Ok(valid) => {
                text.push_str(valid);
                pending.clear();
                break;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                text.push_str(std::str::from_utf8(&pending[..valid]).unwrap());
                pending.drain(..valid);
                match error.error_len() {
                    Some(length) => {
                        text.push('\u{fffd}');
                        pending.drain(..length);
                    }
                    None => {
                        if eof {
                            text.push('\u{fffd}');
                            pending.clear();
                        }
                        break;
                    }
                }
            }
        }
    }
    text
}

impl PtyManager {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn create_session(
        &self,
        cwd: Option<String>,
        shell: Option<String>,
    ) -> Result<PtySession> {
        let session = tokio::task::spawn_blocking(move || -> Result<ManagedSession> {
            let cwd = cwd
                .map(std::path::PathBuf::from)
                .unwrap_or(std::env::current_dir()?);
            if !cwd.is_dir() {
                return Err(TerminalError::Invalid(
                    "Terminal working directory does not exist".into(),
                )
                .into());
            }
            let cwd = std::fs::canonicalize(cwd)?;
            #[cfg(windows)]
            let cwd = {
                // PowerShell treats extended-length cwd paths as provider-qualified paths,
                // which breaks ordinary filesystem commands and .NET path arguments.
                let path = cwd.to_string_lossy();
                std::path::PathBuf::from(
                    path.strip_prefix(r"\\?\UNC\")
                        .map(|unc| format!(r"\\{unc}"))
                        .unwrap_or_else(|| path.strip_prefix(r"\\?\").unwrap_or(&path).to_owned()),
                )
            };
            let shell = shell.unwrap_or_else(|| {
                if cfg!(windows) {
                    "powershell.exe".into()
                } else {
                    std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into())
                }
            });
            if shell.trim().is_empty() {
                return Err(TerminalError::Invalid("Terminal shell cannot be empty".into()).into());
            }
            let pair = native_pty_system().openpty(PtySize::default())?;
            let mut reader = pair.master.try_clone_reader()?;
            let writer = pair.master.take_writer()?;
            let mut command = CommandBuilder::new(&shell);
            command.cwd(&cwd);
            command.env("TERM", "xterm-256color");
            let mut child = pair.slave.spawn_command(command)?;
            #[cfg(windows)]
            let job = match TerminalJob::new(child.as_ref()) {
                Ok(job) => job,
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(error);
                }
            };
            drop(pair.slave);
            let info = PtySession {
                id: uuid::Uuid::new_v4().to_string(),
                cwd: cwd.to_string_lossy().into_owned(),
                pid: child.process_id().unwrap_or(0),
                shell,
            };
            let (tx, _) = broadcast::channel(256);
            let state = Arc::new(Mutex::new(OutputState {
                tx,
                replay: VecDeque::new(),
                bytes: 0,
                finished: false,
            }));
            let child = Arc::new(Mutex::new(child));
            let (reader_done, done) = std::sync::mpsc::channel();
            let reader_state = state.clone();
            let id = info.id.clone();
            std::thread::spawn(move || {
                let mut bytes = [0u8; 8192];
                let mut pending = Vec::new();
                loop {
                    let count = match reader.read(&mut bytes) {
                        Ok(count) => count,
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => 0,
                    };
                    pending.extend_from_slice(&bytes[..count]);
                    let text = decode_utf8(&mut pending, count == 0);
                    if !text.is_empty() {
                        reader_state
                            .lock()
                            .unwrap()
                            .publish(output(&id, "data", text, None));
                    }
                    if count == 0 {
                        break;
                    }
                }
                let _ = reader_done.send(());
            });
            let wait_child = child.clone();
            let wait_state = state.clone();
            let id = info.id.clone();
            std::thread::spawn(move || {
                let result = loop {
                    let status = wait_child.lock().unwrap().try_wait();
                    match status {
                        Ok(Some(status)) => break Ok(status),
                        Err(error) => break Err(error),
                        Ok(None) => std::thread::sleep(std::time::Duration::from_millis(25)),
                    }
                };
                // Drain final PTY output before publishing the authoritative process exit.
                let _ = done.recv_timeout(std::time::Duration::from_secs(2));
                let event = match result {
                    Ok(status) => output(
                        &id,
                        "exit",
                        status.signal().unwrap_or_default().into(),
                        Some(status.exit_code()),
                    ),
                    Err(error) => output(&id, "error", error.to_string(), None),
                };
                wait_state.lock().unwrap().publish(event);
            });
            Ok(ManagedSession {
                info,
                master: Mutex::new(pair.master),
                writer: Mutex::new(writer),
                child,
                output: state,
                #[cfg(windows)]
                job,
            })
        })
        .await??;
        let info = session.info.clone();
        self.sessions
            .lock()
            .unwrap()
            .insert(info.id.clone(), Arc::new(session));
        Ok(info)
    }

    fn session(&self, id: &str) -> Result<Arc<ManagedSession>> {
        self.sessions
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| TerminalError::NotFound.into())
    }

    pub async fn write_input(&self, id: &str, data: &str) -> Result<()> {
        let session = self.session(id)?;
        let data = data.to_owned();
        tokio::task::spawn_blocking(move || -> Result<()> {
            if session.child.lock().unwrap().try_wait()?.is_some() {
                return Err(TerminalError::Exited.into());
            }
            let mut writer = session.writer.lock().unwrap();
            writer.write_all(data.as_bytes())?;
            writer.flush()?;
            Ok(())
        })
        .await?
    }

    pub async fn resize(&self, id: &str, cols: u16, rows: u16) -> Result<()> {
        if cols == 0 || rows == 0 || cols > 4096 || rows > 4096 {
            return Err(TerminalError::Invalid(
                "Terminal dimensions must be between 1 and 4096".into(),
            )
            .into());
        }
        let session = self.session(id)?;
        tokio::task::spawn_blocking(move || -> Result<()> {
            session.master.lock().unwrap().resize(PtySize {
                cols,
                rows,
                pixel_width: 0,
                pixel_height: 0,
            })?;
            Ok(())
        })
        .await?
    }

    pub async fn close_session(&self, id: &str) -> Result<()> {
        let session = self.session(id)?;
        tokio::task::spawn_blocking(move || session.stop()).await??;
        let session = self.sessions.lock().unwrap().remove(id);
        // Dropping ConPTY can block while it flushes output; keep it off the async executor.
        tokio::task::spawn_blocking(move || drop(session)).await?;
        Ok(())
    }

    pub async fn list_sessions(&self) -> Vec<PtySession> {
        self.sessions
            .lock()
            .unwrap()
            .values()
            .map(|session| session.info.clone())
            .collect()
    }

    pub async fn subscribe(&self, id: &str) -> Option<PtySubscription> {
        let session = self.session(id).ok()?;
        let state = session.output.lock().unwrap();
        // Snapshot and subscription are atomic relative to publication: no lost or duplicate chunks.
        Some(PtySubscription {
            replay: state.replay.iter().cloned().collect(),
            receiver: state.tx.subscribe(),
        })
    }
}

impl Default for PtyManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf8_split_and_invalid_sequences() {
        let text = "hello 中文🙂";
        let mut pending = Vec::new();
        let mut decoded = String::new();
        for byte in text.as_bytes() {
            pending.push(*byte);
            decoded.push_str(&decode_utf8(&mut pending, false));
        }
        assert_eq!(decoded, text);
        pending.extend_from_slice(&[0xff, 0xe4]);
        assert_eq!(decode_utf8(&mut pending, false), "\u{fffd}");
        assert_eq!(decode_utf8(&mut pending, true), "\u{fffd}");
    }

    #[test]
    fn replay_is_bounded_and_keeps_exit_status() {
        let (tx, _) = broadcast::channel(2);
        let mut state = OutputState {
            tx,
            replay: VecDeque::new(),
            bytes: 0,
            finished: false,
        };
        for _ in 0..100 {
            state.publish(output("fixture", "data", "中".repeat(4096), None));
        }
        state.publish(output("fixture", "exit", String::new(), Some(7)));
        state.publish(output("fixture", "data", "late".into(), None));
        assert!(state.bytes <= REPLAY_BYTES);
        assert_eq!(state.replay.back().unwrap().code, Some(7));
    }

    #[tokio::test]
    async fn missing_sessions_and_invalid_creation_are_errors() {
        let manager = PtyManager::new();
        assert!(manager.write_input("missing", "hello").await.is_err());
        assert!(manager.resize("missing", 80, 24).await.is_err());
        assert!(manager.close_session("missing").await.is_err());
        assert!(manager
            .create_session(Some("/nonexistent-terminal-fixture-directory".into()), None)
            .await
            .is_err());
        assert!(manager.list_sessions().await.is_empty());
    }

    #[tokio::test]
    async fn real_shell_input_resize_replay_and_nonzero_exit() {
        let directory = tempfile::tempdir().unwrap();
        let manager = PtyManager::new();
        let session = manager
            .create_session(Some(directory.path().to_string_lossy().into_owned()), None)
            .await
            .unwrap();
        assert_ne!(session.pid, 0);
        manager.resize(&session.id, 100, 35).await.unwrap();
        assert!(manager.resize(&session.id, 0, 35).await.is_err());
        let command = if cfg!(windows) {
            "[Console]::WriteLine(('REAL_' + 'PTY_42')); exit 7\r"
        } else {
            "printf 'REAL_%s\\n' PTY_42; exit 7\n"
        };
        manager.write_input(&session.id, command).await.unwrap();
        let subscription = manager.subscribe(&session.id).await.unwrap();
        let mut events = subscription.replay;
        let mut receiver = subscription.receiver;
        tokio::time::timeout(std::time::Duration::from_secs(20), async {
            while !events.iter().any(|event| event.kind == "exit") {
                events.push(receiver.recv().await.unwrap());
            }
        })
        .await
        .unwrap();
        let text: String = events
            .iter()
            .filter(|event| event.kind == "data")
            .map(|event| event.data.as_str())
            .collect();
        assert!(
            text.contains("REAL_PTY_42"),
            "Output did not contain executed result: {text}"
        );
        assert_eq!(events.last().unwrap().code, Some(7));
        assert!(manager.write_input(&session.id, "late").await.is_err());
        let replay = manager.subscribe(&session.id).await.unwrap();
        assert_eq!(replay.replay.last().unwrap().code, Some(7));
        manager.close_session(&session.id).await.unwrap();
        assert!(manager.list_sessions().await.is_empty());
        assert!(manager.subscribe(&session.id).await.is_none());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn close_terminates_owned_windows_descendant() {
        use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0, WAIT_TIMEOUT};
        use windows::Win32::System::Threading::{
            OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
        };
        let directory = tempfile::tempdir().unwrap();
        let manager = PtyManager::new();
        let session = manager
            .create_session(Some(directory.path().to_string_lossy().into_owned()), None)
            .await
            .unwrap();
        manager.write_input(&session.id,
            "$p = Start-Process powershell.exe -ArgumentList '-NoProfile','-Command','Start-Sleep -Seconds 120' -PassThru; [IO.File]::WriteAllText((Join-Path $PWD 'child.pid'), [string]$p.Id)\r"
        ).await.unwrap();
        let pid_file = directory.path().join("child.pid");
        let pid = tokio::time::timeout(std::time::Duration::from_secs(20), async {
            loop {
                if let Ok(text) = std::fs::read_to_string(&pid_file) {
                    if let Ok(pid) = text.trim().parse::<u32>() {
                        break pid;
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await;
        let pid = match pid {
            Ok(pid) => pid,
            Err(error) => {
                let output: String = manager
                    .subscribe(&session.id)
                    .await
                    .unwrap()
                    .replay
                    .iter()
                    .map(|event| event.data.as_str())
                    .collect();
                manager.close_session(&session.id).await.unwrap();
                panic!("Fixture child did not report its PID: {error}; PTY output: {output}");
            }
        };
        // Retain a handle to this fixture child, never enumerate or terminate unrelated processes.
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid).unwrap() };
        let before_close = unsafe { WaitForSingleObject(handle, 0) };
        let closed = manager.close_session(&session.id).await;
        let after_close = unsafe { WaitForSingleObject(handle, 5000) };
        unsafe {
            CloseHandle(handle).unwrap();
        }
        closed.unwrap();
        assert_eq!(
            before_close, WAIT_TIMEOUT,
            "Fixture child must still be alive"
        );
        assert_eq!(after_close, WAIT_OBJECT_0, "Close must terminate the child");
    }

    #[tokio::test]
    async fn close_reaps_only_owned_shells_and_can_repeat() {
        let directory = tempfile::tempdir().unwrap();
        let manager = PtyManager::new();
        for _ in 0..3 {
            let session = manager
                .create_session(Some(directory.path().to_string_lossy().into_owned()), None)
                .await
                .unwrap();
            let owned = manager.session(&session.id).unwrap();
            manager.close_session(&session.id).await.unwrap();
            assert!(owned.child.lock().unwrap().try_wait().unwrap().is_some());
            drop(owned);
        }
        assert!(manager.list_sessions().await.is_empty());
    }
}

pub async fn execute_bash_command(
    command: &str,
    cwd: Option<&str>,
    timeout_secs: u64,
    env_vars: Option<HashMap<String, String>>,
) -> Result<String> {
    let cwd = cwd.unwrap_or(".");
    let is_win = cfg!(windows);

    let mut cmd = if is_win {
        let mut c = Command::new("powershell");
        c.args(["-Command", command]);
        c
    } else {
        let mut c = Command::new("bash");
        c.args(["-c", command]);
        c
    };

    cmd.current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    if let Some(env) = env_vars {
        let envs: Vec<(String, String)> = env
            .into_iter()
            .chain([
                ("LANG".to_string(), "en_US.UTF-8".to_string()),
                ("TERM".to_string(), "xterm-256color".to_string()),
            ])
            .collect();
        cmd.envs(envs);
    } else {
        cmd.env("LANG", "en_US.UTF-8");
        cmd.env("TERM", "xterm-256color");
    }

    let timeout = tokio::time::Duration::from_secs(timeout_secs);

    let result = tokio::time::timeout(timeout, cmd.output()).await;

    match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);

            let mut result = stdout.to_string();
            if !stderr.is_empty() {
                result.push_str(&format!("\nSTDERR: {}", stderr));
            }

            if !output.status.success() && stdout.is_empty() {
                result = format!(
                    "Command exited with code {:?}: {}",
                    output.status.code(),
                    stderr
                );
            }

            Ok(result)
        }
        Ok(Err(e)) => Err(anyhow::anyhow!("Failed to execute command: {}", e)),
        Err(_) => Err(anyhow::anyhow!(
            "Command timed out after {} seconds",
            timeout_secs
        )),
    }
}
