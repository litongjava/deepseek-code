//! Stdio ownership follows an inherited environment entry across fork, exec and
//! setsid. Parentage and the original process group also cover observed children
//! that subsequently exec with a clean environment.
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fs, io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    os::unix::fs::MetadataExt,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};
use uuid::Uuid;

mod fences;
pub(crate) use fences::drain_predecessors;

const INSTANCE_ENV: &str = "DEEPSEEK_CODE_MCP_INSTANCE";
const TERM_GRACE: Duration = Duration::from_millis(250);
const STOP_BOUND: Duration = Duration::from_secs(3);
const POLL: Duration = Duration::from_millis(10);
struct Owner {
    id: Uuid,
    supervisor_pid: Option<u32>,
}

static OWNER: OnceLock<Owner> = OnceLock::new();

fn owner_uuid(worker_id: &str) -> Uuid {
    if let Some(id) = Uuid::parse_str(worker_id)
        .ok()
        .filter(|id| id.to_string() == worker_id)
    {
        return id;
    }

    // The namespace and hash must stay stable across engine and supervisor restarts.
    let digest = Sha256::new()
        .chain_update(b"deepseek-code:mcp:worker-owner\0")
        .chain_update(worker_id.as_bytes())
        .finalize();
    let mut bytes = [0; 16];
    bytes.copy_from_slice(&digest[..16]);
    uuid::Builder::from_custom_bytes(bytes).into_uuid()
}

fn instance_token(owner: Uuid, instance: Uuid) -> String {
    format!("{owner}:{instance}")
}

fn worker_prefix(owner: Uuid) -> String {
    format!("{INSTANCE_ENV}={owner}:")
}

/// Run once in the worker before constructing any stdio connectors. Only the
/// engine and its supervisor need the worker ID. Other hosts get a local UUID.
pub fn init_worker_mcp(
    worker_id: &str,
    session_dir: &std::path::Path,
    supervisor_pid: Option<u32>,
) -> anyhow::Result<()> {
    let owner = Owner {
        id: owner_uuid(worker_id),
        supervisor_pid,
    };
    anyhow::ensure!(
        OWNER.set(owner).is_ok(),
        "MCP process owner already initialized"
    );
    fences::initialize(session_dir)
}

pub(super) fn spawn(
    command: &mut tokio::process::Command,
    server: &str,
) -> anyhow::Result<(tokio::process::Child, Tracker)> {
    let owner = OWNER
        .get_or_init(|| Owner {
            id: Uuid::new_v4(),
            supervisor_pid: None,
        })
        .id;
    let instance = Uuid::new_v4();
    let fence = fences::reserve(server, owner, instance)?;
    let token = instance_token(owner, instance);
    command.env(INSTANCE_ENV, &token);
    let child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            if let Some(fence) = fence {
                fence.clear_reservation()?;
            }
            return Err(error.into());
        }
    };
    let pid = child.id().expect("just spawned") as i32;
    Ok((
        child,
        Tracker::new(pid, format!("{INSTANCE_ENV}={token}"), fence),
    ))
}

#[derive(Debug)]
struct Stat {
    state: u8,
    parent: i32,
    group: i32,
    birth: u64,
}

fn proc_file(pid: i32, file: &str) -> io::Result<Vec<u8>> {
    fs::read(format!("/proc/{pid}/{file}")).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            io::Error::from_raw_os_error(libc::ESRCH)
        } else {
            error
        }
    })
}

fn stat(pid: i32) -> io::Result<Stat> {
    let bytes = proc_file(pid, "stat")?;
    parse_stat(&bytes)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid /proc stat"))
}

fn parse_stat(bytes: &[u8]) -> Option<Stat> {
    // comm is arbitrary bytes and can contain spaces and parentheses.
    let close = bytes.iter().rposition(|&b| b == b')')?;
    let mut fields = std::str::from_utf8(&bytes[close + 1..])
        .ok()?
        .split_whitespace();
    let state = fields.next()?.as_bytes().first().copied()?;
    let parent = fields.next()?.parse().ok()?;
    let group = fields.next()?.parse().ok()?;
    let birth = fields.nth(16)?.parse().ok()?;
    Some(Stat {
        state,
        parent,
        group,
        birth,
    })
}

fn children(pid: i32, deadline: Instant) -> io::Result<Vec<i32>> {
    let mut result = Vec::new();
    for task in fs::read_dir(format!("/proc/{pid}/task"))? {
        check_deadline(deadline)?;
        let task = task?;
        match fs::read_to_string(task.path().join("children")) {
            Ok(ids) => result.extend(ids.split_whitespace().filter_map(|s| s.parse::<i32>().ok())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(result)
}

#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
struct Members(HashMap<i32, u64>);

impl Members {
    fn refresh(&mut self, deadline: Instant) {
        self.0.retain(|&pid, born| {
            Instant::now() >= deadline
                || match stat(pid) {
                    Ok(s) => s.birth == *born,
                    Err(e) => e.raw_os_error() != Some(libc::ESRCH),
                }
        });
        let mut pending: Vec<_> = self.0.keys().copied().collect();
        let mut visited = HashSet::new();
        while let Some(pid) = pending.pop() {
            if Instant::now() >= deadline {
                break;
            }
            if !visited.insert(pid) {
                continue;
            }
            let Ok(ids) = children(pid, deadline) else {
                continue;
            };
            if !stat(pid).is_ok_and(|s| self.0.get(&pid) == Some(&s.birth)) {
                continue;
            }
            for child in ids {
                if Instant::now() >= deadline {
                    return;
                }
                if let Ok(s) = stat(child) {
                    if s.parent == pid && s.state != b'Z' {
                        self.0.insert(child, s.birth);
                        pending.push(child);
                    }
                }
            }
        }
    }

    fn scan(&mut self, scope: &Scope<'_>, deadline: Instant) -> io::Result<bool> {
        let mut found_token = false;
        let uid = unsafe { libc::geteuid() };
        let mut group_members = Vec::new();
        for entry in fs::read_dir("/proc")? {
            check_deadline(deadline)?;
            let entry = entry?;
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
                continue;
            };
            let path = entry.path();
            if !fs::metadata(&path).is_ok_and(|m| m.uid() == uid) {
                continue;
            }
            let Ok(s) = stat(pid) else { continue };
            if scope.launcher() == Some(s.group) {
                group_members.push((pid, s.birth));
            }
            // Disappearing PIDs, zombies and processes with restricted environ
            // access are normal in a system-wide scan. Keep known tree members.
            let env = match fs::read(path.join("environ")) {
                Ok(env) => env,
                Err(e)
                    if matches!(
                        e.raw_os_error(),
                        Some(libc::ENOENT | libc::ESRCH | libc::EACCES | libc::EPERM | libc::EIO)
                    ) =>
                {
                    continue
                }
                Err(e) => return Err(e),
            };
            if env.split(|&b| b == 0).any(|entry| scope.matches(entry)) {
                found_token = true;
                self.0.insert(pid, s.birth);
            }
        }
        // An old numeric PGID can be reused after the launcher is reaped.
        // Accept the group only while it contains an identified member.
        if group_members
            .iter()
            .any(|(pid, birth)| self.0.get(pid) == Some(birth))
        {
            self.0.extend(group_members);
        }
        Ok(found_token)
    }

    fn signal_member(
        &self,
        scope: &Scope<'_>,
        pid: i32,
        birth: u64,
        signal: i32,
        termed: &mut HashSet<(i32, u64)>,
        signals: &dyn Signals,
    ) -> io::Result<bool> {
        // A pidfd pins the candidate while we verify its original start time.
        let fd = signals.open(pid);
        let s = stat(pid)?;
        if s.birth != birth {
            return Ok(true);
        }
        if s.state == b'Z' {
            if Some(pid) == scope.launcher() {
                return Ok(false); // tokio's Child owns this wait
            }
            if s.parent == std::process::id() as i32 {
                let waited = unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
                if waited < 0 {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() != Some(libc::ECHILD) {
                        return Err(error);
                    }
                }
                return Ok(waited != 0);
            }
            // The supervisor reaps detached descendants on its own heartbeat.
            // Keep the instance fenced until the old PID actually disappears.
            if OWNER.get().and_then(|owner| owner.supervisor_pid) == Some(s.parent as u32) {
                return Ok(false);
            }
            return Ok(!self.0.contains_key(&s.parent)); // might still await adoption
        }
        if signal == libc::SIGTERM && termed.contains(&(pid, birth)) {
            return Ok(false);
        }
        if let Err(error) = fd.and_then(|fd| signals.send(&fd, signal)) {
            if !fallback_allowed(&error) {
                return Err(error);
            }
            let env = proc_file(pid, "environ")?;
            if !env.split(|&b| b == 0).any(|entry| scope.matches(entry)) {
                // Known descendants may have cleared their token. Retain them
                // as unresolved rather than sending a numeric signal blindly.
                return Err(io::Error::new(
                    error.kind(),
                    format!("pidfd: {error}, cannot verify MCP environment token for PID {pid}"),
                ));
            }
            if stat(pid)?.birth != birth {
                return Ok(true);
            }
            signals.kill(pid, signal)?;
        }
        if signal == libc::SIGTERM {
            termed.insert((pid, birth));
        }
        Ok(false)
    }
}

fn fallback_allowed(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EPERM | libc::ENOSYS | libc::EINVAL)
    )
}

trait Signals {
    fn open(&self, pid: i32) -> io::Result<OwnedFd> {
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        }
    }

    fn send(&self, fd: &OwnedFd, signal: i32) -> io::Result<()> {
        let rc = unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                fd.as_raw_fd(),
                signal,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            )
        };
        if rc < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn kill(&self, pid: i32, signal: i32) -> io::Result<()> {
        if unsafe { libc::kill(pid, signal) } < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

struct Kernel;
impl Signals for Kernel {}

fn check_deadline(deadline: Instant) -> io::Result<()> {
    if Instant::now() >= deadline {
        Err(io::Error::from_raw_os_error(libc::ETIMEDOUT))
    } else {
        Ok(())
    }
}

enum Scope<'a> {
    Instance { entry: &'a str, launcher: i32 },
    Worker { prefix: &'a str },
    Predecessor { entry: &'a str },
}

impl Scope<'_> {
    fn matches(&self, env: &[u8]) -> bool {
        match self {
            Self::Instance { entry, .. } | Self::Predecessor { entry } => env == entry.as_bytes(),
            Self::Worker { prefix } => env.starts_with(prefix.as_bytes()),
        }
    }

    fn launcher(&self) -> Option<i32> {
        match self {
            Self::Instance { launcher, .. } => Some(*launcher),
            Self::Worker { .. } | Self::Predecessor { .. } => None,
        }
    }
}

fn drain(
    members: &mut Members,
    scope: Scope<'_>,
    signals: &dyn Signals,
    mut reap: impl FnMut(Instant) -> io::Result<bool>,
) -> io::Result<()> {
    let start = Instant::now();
    let deadline = start + STOP_BOUND;
    let mut delay = POLL;
    let mut termed = HashSet::new();
    let mut previously_empty = false;
    let mut last_error = io::Error::from_raw_os_error(libc::ETIMEDOUT);
    while Instant::now() < deadline {
        let result = (|| -> io::Result<bool> {
            members.refresh(deadline);
            let found_token = members.scan(&scope, deadline)?;
            if let Scope::Worker { .. } = scope {
                // The supervisor also owns untagged adopted children, including
                // the engine if supervision returned on an error.
                for pid in children(std::process::id() as i32, deadline)? {
                    check_deadline(deadline)?;
                    if let Ok(s) = stat(pid) {
                        members.0.insert(pid, s.birth);
                    }
                }
            }
            let signal = if start.elapsed() < TERM_GRACE {
                libc::SIGTERM
            } else {
                libc::SIGKILL
            };
            let mut stopped = true;
            for (&pid, &birth) in &members.0 {
                check_deadline(deadline)?;
                match members.signal_member(&scope, pid, birth, signal, &mut termed, signals) {
                    Ok(done) => stopped &= done,
                    Err(e) if e.raw_os_error() == Some(libc::ESRCH) => {}
                    Err(e) => {
                        stopped = false;
                        last_error = e;
                    }
                }
            }
            Ok(reap(deadline)? && stopped && !found_token)
        })();
        let empty = match result {
            Ok(empty) => empty,
            Err(e) => {
                if e.raw_os_error() != Some(libc::ETIMEDOUT) {
                    last_error = e;
                }
                false
            }
        };
        // Confirm an empty pass after a final fork/exec or reparent.
        if empty && previously_empty {
            return Ok(());
        }
        previously_empty = empty;
        std::thread::sleep(delay.min(deadline.saturating_duration_since(Instant::now())));
        delay = (delay * 2).min(Duration::from_millis(200));
    }
    let mut pids: Vec<_> = members.0.keys().copied().collect();
    pids.sort_unstable();
    Err(io::Error::new(
        last_error.kind(),
        format!("PIDs {pids:?}: {last_error}"),
    ))
}

pub(super) struct Tracker {
    token: String,
    fence: Option<fences::Fence>,
    launcher: i32,
    members: Arc<Mutex<Members>>,
    monitor: tokio::task::JoinHandle<()>,
    #[cfg(test)]
    signals: Box<dyn Signals + Send + Sync>,
}

impl Tracker {
    fn new(pid: i32, token: String, fence: Option<fences::Fence>) -> Self {
        let mut initial = Members::default();
        if let Ok(s) = stat(pid) {
            initial.0.insert(pid, s.birth);
        }
        let members = Arc::new(Mutex::new(initial));
        let tracked = members.clone();
        let monitor = tokio::spawn(async move {
            loop {
                tracked
                    .lock()
                    .unwrap()
                    .refresh(Instant::now() + Duration::from_millis(100));
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
        Self {
            token,
            fence,
            launcher: pid,
            members,
            monitor,
            #[cfg(test)]
            signals: Box::new(Kernel),
        }
    }

    pub(super) fn stop(&mut self, child: &mut tokio::process::Child) -> anyhow::Result<()> {
        self.monitor.abort();
        let mut members = self.members.lock().unwrap();
        let result = drain(
            &mut members,
            Scope::Instance {
                entry: &self.token,
                launcher: self.launcher,
            },
            #[cfg(test)]
            self.signals.as_ref(),
            #[cfg(not(test))]
            &Kernel,
            |_| Ok(child.try_wait()?.is_some()),
        );
        if let Some(fence) = &mut self.fence {
            fence.finish(&result, &members)?;
        }
        Ok(result?)
    }
}

/// After engine death the supervisor has no connector handles. Its worker UUID
/// covers every instance, even one whose launcher exited before registration.
pub(crate) fn drain_worker(worker_id: &str, session_dir: &std::path::Path) -> anyhow::Result<()> {
    let owner = owner_uuid(worker_id);
    let prefix = worker_prefix(owner);
    let mut members = Members::default();
    let result = drain(
        &mut members,
        Scope::Worker { prefix: &prefix },
        &Kernel,
        |deadline| {
            // ECHILD confirms the kernel finished reparenting the worker's children.
            loop {
                check_deadline(deadline)?;
                match unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) } {
                    0 => return Ok(false),
                    -1 => {
                        let error = io::Error::last_os_error();
                        return if error.raw_os_error() == Some(libc::ECHILD) {
                            Ok(true)
                        } else {
                            Err(error)
                        };
                    }
                    _ => {}
                }
            }
        },
    );
    fences::supervisor_finished(session_dir, owner, &members, result.is_ok())?;
    Ok(result?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::process::ServerProcess;

    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn worker_owner_preserves_canonical_uuids_and_maps_free_form_ids_stably() {
        let canonical = "01234567-89ab-4def-8123-456789abcdef";
        let id = Uuid::parse_str(canonical).unwrap();
        assert_eq!(owner_uuid(canonical), id);
        assert_ne!(owner_uuid(&id.simple().to_string()), id);
        assert_ne!(owner_uuid(&canonical.to_uppercase()), id);

        let worker = "sentinel-free-form-worker-id";
        assert_eq!(
            owner_uuid(worker).to_string(),
            "8f17e8bd-5743-8b59-ad91-dffccf748660"
        );
        assert_ne!(owner_uuid(worker), owner_uuid("another-free-form-worker"));
    }

    #[test]
    fn supervisor_prefix_matches_the_free_form_workers_instance_token() {
        let worker = "sentinel-free-form-worker-id";
        let token = instance_token(owner_uuid(worker), Uuid::new_v4());
        let entry = format!("{INSTANCE_ENV}={token}");
        let prefix = worker_prefix(owner_uuid(worker));
        assert!(Scope::Worker { prefix: &prefix }.matches(entry.as_bytes()));
        let other = worker_prefix(owner_uuid("another-free-form-worker"));
        assert!(!Scope::Worker { prefix: &other }.matches(entry.as_bytes()));
    }

    struct Denied {
        deny_open: bool,
        deny_kill: bool,
        errno: i32,
        kills: AtomicUsize,
    }

    impl Signals for Denied {
        fn open(&self, _: i32) -> io::Result<OwnedFd> {
            if self.deny_open {
                Err(io::Error::from_raw_os_error(self.errno))
            } else {
                // send() is injected too, so this path works on pre-pidfd kernels.
                Ok(fs::File::open("/dev/null")?.into())
            }
        }

        fn send(&self, _: &OwnedFd, _: i32) -> io::Result<()> {
            Err(io::Error::from_raw_os_error(self.errno))
        }

        fn kill(&self, pid: i32, signal: i32) -> io::Result<()> {
            self.kills.fetch_add(1, Ordering::Relaxed);
            if self.deny_kill {
                Err(io::Error::from_raw_os_error(libc::EPERM))
            } else {
                Kernel.kill(pid, signal)
            }
        }
    }

    #[tokio::test]
    async fn denied_pidfd_open_or_send_falls_back_to_identity_checked_kill() {
        use std::os::unix::process::ExitStatusExt;
        for errno in [libc::EPERM, libc::ENOSYS, libc::EINVAL] {
            for deny_open in [true, false] {
                let mut cmd = tokio::process::Command::new("/bin/sleep");
                let mut server = ServerProcess::spawn(cmd.arg("60"), "w").unwrap();
                let pid = server.tree.launcher;
                let birth = stat(pid).unwrap().birth;
                let scope = Scope::Instance {
                    entry: &server.tree.token,
                    launcher: pid,
                };
                let denied = Denied {
                    deny_open,
                    deny_kill: false,
                    errno,
                    kills: 0.into(),
                };
                let members = Members::default();
                let mut termed = HashSet::new();
                let foreign = Scope::Instance {
                    entry: "wrong-token",
                    launcher: pid,
                };
                assert!(members
                    .signal_member(&foreign, pid, birth, libc::SIGTERM, &mut termed, &denied)
                    .is_err());
                assert!(members
                    .signal_member(&scope, pid, birth + 1, libc::SIGTERM, &mut termed, &denied)
                    .unwrap());
                assert_eq!(denied.kills.load(Ordering::Relaxed), 0);
                drain(&mut Members::default(), scope, &denied, |_| {
                    Ok(server.child.try_wait()?.is_some())
                })
                .unwrap();
                assert_eq!(denied.kills.load(Ordering::Relaxed), 1);
                assert_eq!(
                    server.child.try_wait().unwrap().unwrap().signal(),
                    Some(libc::SIGTERM)
                );
            }
        }
    }

    #[tokio::test]
    async fn refused_signals_return_pid_and_errno_within_the_drain_bound() {
        let mut cmd = tokio::process::Command::new("/bin/sleep");
        let mut server = ServerProcess::spawn(cmd.arg("60"), "w").unwrap();
        let pid = server.tree.launcher;
        let denied = Denied {
            deny_open: true,
            deny_kill: true,
            errno: libc::EPERM,
            kills: 0.into(),
        };
        let before = Instant::now();
        let error = drain(
            &mut Members::default(),
            Scope::Instance {
                entry: &server.tree.token,
                launcher: pid,
            },
            &denied,
            |_| Ok(server.child.try_wait()?.is_some()),
        )
        .unwrap_err();
        assert!(
            before.elapsed() >= STOP_BOUND
                && before.elapsed() < STOP_BOUND + Duration::from_secs(1)
        );
        assert!(
            (2..30).contains(&denied.kills.load(Ordering::Relaxed)),
            "{}",
            denied.kills.load(Ordering::Relaxed)
        );
        assert!(
            error.to_string().contains(&pid.to_string())
                && error.to_string().contains("os error 1"),
            "{error}"
        );
        assert!(server.child.try_wait().unwrap().is_none());
        server.stop().unwrap();
    }

    #[tokio::test]
    async fn refused_stop_fences_the_connector_until_a_successful_retry() {
        use crate::mcp::McpServerManager;
        use serde_json::json;
        use std::path::Path;

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("mcp.json");
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/live-mcp.cjs");
        let config = json!({"mcpServers": {"w": {
            "command":"node", "args":["-e", "setInterval(() => {}, 60000); require(process.argv[1])", fixture],
            "enabled":true
        }}});
        fs::write(&path, config.to_string()).unwrap();
        let manager = McpServerManager::new(path.clone());
        manager.load_config().await.unwrap();
        manager.start_server("w").await.unwrap();
        let connector = manager.servers.read().await["w"].connector.clone().unwrap();
        let pid = {
            let mut guard = connector.lock().await;
            let tree = &mut guard.process.as_mut().unwrap().tree;
            tree.signals = Box::new(Denied {
                deny_open: true,
                deny_kill: true,
                errno: libc::EPERM,
                kills: 0.into(),
            });
            tree.launcher
        };
        let error = manager
            .set_server_enabled("w", false)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("MCP server w could not be stopped")
                && error.contains(&pid.to_string())
                && error.contains("os error 1"),
            "{error}"
        );
        let saved: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(saved["mcpServers"]["w"]["enabled"], false);
        let status = manager.get_server("w").await.unwrap();
        assert!(status.enabled && status.running);
        assert_eq!(status.error.as_deref(), Some(error.as_str()));
        assert_eq!(
            manager.reconcile_config().await.unwrap_err().to_string(),
            error
        );
        assert_eq!(
            manager
                .set_server_enabled("w", true)
                .await
                .unwrap_err()
                .to_string(),
            error
        );
        assert_eq!(
            manager.start_server("w").await.unwrap_err().to_string(),
            error
        );
        assert_eq!(
            manager.restart_server("w").await.unwrap_err().to_string(),
            error
        );
        assert_eq!(
            manager.remove_server("w").await.unwrap_err().to_string(),
            error
        );
        assert!(Arc::ptr_eq(
            manager.servers.read().await["w"]
                .connector
                .as_ref()
                .unwrap(),
            &connector
        ));
        assert!(stat(pid).is_ok_and(|s| s.state != b'Z'));
        connector
            .lock()
            .await
            .process
            .as_mut()
            .unwrap()
            .tree
            .signals = Box::new(Kernel);
        manager.stop_server("w").await.unwrap();
        assert_eq!(stat(pid).unwrap_err().raw_os_error(), Some(libc::ESRCH));
        assert!(manager.servers.read().await["w"].connector.is_none());
        let status = manager.get_server("w").await.unwrap();
        assert!(!status.enabled && !status.running && status.error.is_none());
        manager.set_server_enabled("w", true).await.unwrap();
        manager.start_server("w").await.unwrap();
        manager.remove_server("w").await.unwrap();
        assert!(manager.get_server("w").await.is_none());
    }

    #[tokio::test]
    async fn token_stop_reaches_a_peer_spares_a_near_match_and_escalates_after_term() {
        use std::os::unix::process::ExitStatusExt;

        let tmp = tempfile::tempdir().unwrap();
        let mut command = tokio::process::Command::new("/bin/sh");
        command.current_dir(tmp.path()).args([
            "-c",
            "trap 'echo TERM > signal' TERM; touch ready; while :; do sleep 0.02; done",
        ]);
        let mut server = ServerProcess::spawn(&mut command, "w").unwrap();
        let token = server.tree.token.split_once('=').unwrap().1;
        // Both peers are outside the launcher's group and ancestry.
        let peer = std::process::Command::new("/bin/sleep")
            .env(INSTANCE_ENV, token)
            .arg("10")
            .spawn()
            .unwrap();
        let mut unrelated = std::process::Command::new("/bin/sleep")
            .env(INSTANCE_ENV, format!("{token}-extra"))
            .arg("10")
            .spawn()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while !tmp.path().join("ready").exists() {
                tokio::time::sleep(POLL).await;
            }
        })
        .await
        .unwrap();
        server.tree.stop(&mut server.child).unwrap();
        assert_eq!(
            fs::read_to_string(tmp.path().join("signal"))
                .unwrap()
                .trim(),
            "TERM"
        );
        assert_eq!(
            server.child.try_wait().unwrap().unwrap().signal(),
            Some(libc::SIGKILL)
        );
        assert_eq!(
            stat(peer.id() as i32).err().unwrap().raw_os_error(),
            Some(libc::ESRCH)
        );
        assert!(unrelated.try_wait().unwrap().is_none());
        unrelated.kill().unwrap();
        unrelated.wait().unwrap();
    }

    #[tokio::test]
    async fn stop_reaches_a_previously_observed_reparented_server() {
        let tmp = tempfile::tempdir().unwrap();
        let mut command = tokio::process::Command::new("/bin/sh");
        command.current_dir(tmp.path()).args([
            "-c",
            "setsid sleep 15 & echo $! > server.pid; while [ ! -e release ]; do sleep 0.02; done",
        ]);
        let mut server = ServerProcess::spawn(&mut command, "w").unwrap();
        let pid = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(pid) = fs::read_to_string(tmp.path().join("server.pid"))
                    .ok()
                    .and_then(|id| id.trim().parse().ok())
                {
                    if server.tree.members.lock().unwrap().0.contains_key(&pid)
                        && unsafe { libc::getsid(pid) } == pid
                    {
                        break pid;
                    }
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("monitor did not record the new session");
        fs::write(tmp.path().join("release"), "").unwrap();
        assert!(server.child.wait().await.unwrap().success());
        assert!(stat(pid).is_ok_and(|s| s.state != b'Z' && s.parent != server.tree.launcher));
        drop(server);
        assert!(stat(pid).ok().is_none_or(|s| s.state == b'Z'));
    }
}
