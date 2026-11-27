//! Stdio ownership follows an inherited environment entry across fork, exec and
//! setsid. Parentage and the original process group also cover observed children
//! that subsequently exec with a clean environment.
use std::{
    collections::{HashMap, HashSet},
    fs, io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    os::unix::fs::MetadataExt,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

const INSTANCE_ENV: &str = "DEEPSEEK_CODE_MCP_INSTANCE";
const TERM_GRACE: Duration = Duration::from_millis(250);
const POLL: Duration = Duration::from_millis(10);
static OWNER: OnceLock<uuid::Uuid> = OnceLock::new();

/// Run once in the worker before constructing any stdio connectors. Only the
/// engine and its supervisor need the worker ID. Other hosts get a local UUID.
pub fn init_worker_mcp(worker_id: &str) -> anyhow::Result<()> {
    let id = uuid::Uuid::parse_str(worker_id)?;
    anyhow::ensure!(
        OWNER.set(id).is_ok(),
        "MCP process owner already initialized"
    );
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } == -1 {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

pub(super) fn tag_command(command: &mut tokio::process::Command) -> String {
    let owner = OWNER.get_or_init(uuid::Uuid::new_v4);
    let token = format!("{owner}:{}", uuid::Uuid::new_v4());
    command.env(INSTANCE_ENV, &token);
    format!("{INSTANCE_ENV}={token}")
}

struct Stat {
    state: u8,
    parent: i32,
    group: i32,
    birth: u64,
}

fn stat(pid: i32) -> Option<Stat> {
    let bytes = fs::read(format!("/proc/{pid}/stat")).ok()?;
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

fn children(pid: i32) -> io::Result<Vec<i32>> {
    let mut result = Vec::new();
    for task in fs::read_dir(format!("/proc/{pid}/task"))? {
        let task = task?;
        match fs::read_to_string(task.path().join("children")) {
            Ok(ids) => result.extend(ids.split_whitespace().filter_map(|s| s.parse::<i32>().ok())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(result)
}

#[derive(Default)]
struct Members(HashMap<i32, u64>);

impl Members {
    fn refresh(&mut self) {
        self.0
            .retain(|&pid, born| stat(pid).is_some_and(|s| s.birth == *born));
        let mut pending: Vec<_> = self.0.keys().copied().collect();
        let mut visited = HashSet::new();
        while let Some(pid) = pending.pop() {
            if !visited.insert(pid) {
                continue;
            }
            let Ok(ids) = children(pid) else { continue };
            if !stat(pid).is_some_and(|s| self.0.get(&pid) == Some(&s.birth)) {
                continue;
            }
            for child in ids {
                if let Some(s) = stat(child) {
                    if s.parent == pid && s.state != b'Z' {
                        self.0.insert(child, s.birth);
                        pending.push(child);
                    }
                }
            }
        }
    }

    fn scan(&mut self, scope: &Scope<'_>) -> io::Result<bool> {
        let mut found_token = false;
        let uid = unsafe { libc::geteuid() };
        let mut group_members = Vec::new();
        for entry in fs::read_dir("/proc")? {
            let entry = entry?;
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
                continue;
            };
            let path = entry.path();
            if !fs::metadata(&path).is_ok_and(|m| m.uid() == uid) {
                continue;
            }
            let Some(s) = stat(pid) else { continue };
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

    fn signal(
        &self,
        launcher: Option<i32>,
        signal: i32,
        termed: &mut HashSet<(i32, u64)>,
    ) -> io::Result<bool> {
        let mut stopped = true;
        for (&pid, &birth) in &self.0 {
            // Open first, then check birth. PID reuse during or after the scan
            // cannot redirect a pidfd signal to a different process.
            let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
            let fd = if raw >= 0 {
                Some(unsafe { OwnedFd::from_raw_fd(raw) })
            } else {
                let err = io::Error::last_os_error();
                match err.raw_os_error() {
                    Some(libc::ESRCH) => continue,
                    Some(libc::ENOSYS) => None, // kernels before pidfd_open
                    _ => return Err(err),
                }
            };
            let Some(s) = stat(pid).filter(|s| s.birth == birth) else {
                continue;
            };
            if s.state == b'Z' {
                if Some(pid) == launcher {
                    stopped = false; // tokio's Child owns this wait
                } else if s.parent == std::process::id() as i32 {
                    if unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) } == 0 {
                        stopped = false;
                    }
                } else if self.0.contains_key(&s.parent) {
                    stopped = false; // still waiting for adoption
                }
                continue;
            }
            stopped = false;
            if signal == libc::SIGTERM && termed.contains(&(pid, birth)) {
                continue;
            }
            let result = unsafe {
                if let Some(fd) = fd {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        fd.as_raw_fd(),
                        signal,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    ) as i32
                } else {
                    libc::kill(pid, signal)
                }
            };
            if result < 0 {
                let err = io::Error::last_os_error();
                if err.raw_os_error() != Some(libc::ESRCH) {
                    return Err(err);
                }
            } else if signal == libc::SIGTERM {
                termed.insert((pid, birth));
            }
        }
        Ok(stopped)
    }
}

enum Scope<'a> {
    Instance { entry: &'a str, launcher: i32 },
    Worker { prefix: &'a str },
}

impl Scope<'_> {
    fn matches(&self, env: &[u8]) -> bool {
        match self {
            Self::Instance { entry, .. } => env == entry.as_bytes(),
            Self::Worker { prefix } => env.starts_with(prefix.as_bytes()),
        }
    }

    fn launcher(&self) -> Option<i32> {
        match self {
            Self::Instance { launcher, .. } => Some(*launcher),
            Self::Worker { .. } => None,
        }
    }
}

fn drain(members: &mut Members, scope: Scope<'_>, mut reap: impl FnMut() -> io::Result<bool>) {
    let mut term_started = None;
    let mut termed = HashSet::new();
    let mut previously_empty = false;
    loop {
        let result = (|| -> io::Result<bool> {
            members.refresh();
            let found_token = members.scan(&scope)?;
            if let Scope::Worker { .. } = scope {
                // The dedicated supervisor also owns untagged adopted children,
                // including the engine if supervision returned on an error.
                for pid in children(std::process::id() as i32)? {
                    if let Some(s) = stat(pid) {
                        members.0.insert(pid, s.birth);
                    }
                }
            }
            let signal = if term_started.get_or_insert_with(Instant::now).elapsed() < TERM_GRACE {
                libc::SIGTERM
            } else {
                libc::SIGKILL
            };
            let stopped = members.signal(scope.launcher(), signal, &mut termed)?;
            Ok(reap()? && stopped && !found_token)
        })();
        let empty = result.unwrap_or_else(|error| {
            tracing::warn!(%error, "Checking MCP process ownership");
            false
        });
        // Enumeration can race a final fork/exec or reparent. Confirm an empty
        // pass with a fresh scan before releasing the lifecycle lock.
        if empty && previously_empty {
            return;
        }
        previously_empty = empty;
        std::thread::sleep(POLL);
    }
}

pub(super) struct Tracker {
    token: String,
    launcher: i32,
    members: Arc<Mutex<Members>>,
    monitor: tokio::task::JoinHandle<()>,
}

impl Tracker {
    pub(super) fn new(pid: i32, token: String) -> Self {
        let mut initial = Members::default();
        if let Some(s) = stat(pid) {
            initial.0.insert(pid, s.birth);
        }
        let members = Arc::new(Mutex::new(initial));
        let tracked = members.clone();
        let monitor = tokio::spawn(async move {
            loop {
                tracked.lock().unwrap().refresh();
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
        Self {
            token,
            launcher: pid,
            members,
            monitor,
        }
    }

    pub(super) fn stop(&self, child: &mut tokio::process::Child) {
        self.monitor.abort();
        drain(
            &mut self.members.lock().unwrap(),
            Scope::Instance {
                entry: &self.token,
                launcher: self.launcher,
            },
            || Ok(child.try_wait()?.is_some()),
        );
    }
}

/// After engine death the supervisor has no connector handles. Its worker UUID
/// covers every instance, even one whose launcher exited before registration.
pub(crate) fn drain_worker(worker_id: &str) {
    let prefix = format!("{INSTANCE_ENV}={worker_id}:");
    drain(
        &mut Members::default(),
        Scope::Worker { prefix: &prefix },
        || {
            // ECHILD confirms the kernel finished reparenting the worker's children.
            loop {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::process::ServerProcess;

    #[tokio::test]
    async fn token_stop_reaches_a_peer_spares_a_near_match_and_escalates_after_term() {
        use std::os::unix::process::ExitStatusExt;

        let tmp = tempfile::tempdir().unwrap();
        let mut command = tokio::process::Command::new("/bin/sh");
        command.current_dir(tmp.path()).args([
            "-c",
            "trap 'echo TERM > signal' TERM; touch ready; while :; do sleep 0.02; done",
        ]);
        let mut server = ServerProcess::spawn(&mut command).unwrap();
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
        server.tree.stop(&mut server.child);
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
        assert!(stat(peer.id() as i32).is_none());
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
        let mut server = ServerProcess::spawn(&mut command).unwrap();
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
        assert!(stat(pid).is_some_and(|s| s.state != b'Z' && s.parent != server.tree.launcher));
        drop(server);
        assert!(stat(pid).is_none_or(|s| s.state == b'Z'));
    }
}
