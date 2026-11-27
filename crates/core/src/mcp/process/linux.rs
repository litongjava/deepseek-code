//! Follow parentage, including children forked by non-leader threads. Keep the
//! birth time with each PID so a long-lived connector forgets recycled PIDs.
use std::{
    collections::{HashMap, HashSet},
    fs, io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    sync::{Arc, Mutex},
    time::Duration,
};

fn stat(pid: i32) -> Option<(u8, i32, u64)> {
    let bytes = fs::read(format!("/proc/{pid}/stat")).ok()?;
    // comm is arbitrary bytes and can contain spaces and parentheses.
    let close = bytes.iter().rposition(|&b| b == b')')?;
    let mut fields = std::str::from_utf8(&bytes[close + 1..])
        .ok()?
        .split_whitespace();
    let state = fields.next()?.as_bytes().first().copied()?;
    let parent = fields.next()?.parse().ok()?;
    let birth = fields.nth(17)?.parse().ok()?;
    Some((state, parent, birth))
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
            .retain(|&pid, born| stat(pid).is_some_and(|(_, _, b)| b == *born));
        let mut pending: Vec<_> = self.0.keys().copied().collect();
        let mut visited = HashSet::new();
        while let Some(pid) = pending.pop() {
            if !visited.insert(pid) {
                continue;
            }
            let Ok(ids) = children(pid) else { continue };
            if !stat(pid).is_some_and(|(_, _, b)| self.0.get(&pid) == Some(&b)) {
                continue;
            }
            for child in ids {
                if let Some((state, parent, birth)) = stat(child) {
                    if parent == pid && state != b'Z' {
                        self.0.insert(child, birth);
                        pending.push(child);
                    }
                }
            }
        }
    }

    fn kill(&self, launcher: i32) -> bool {
        let mut stopped = true;
        for (&pid, &birth) in &self.0 {
            // Pin the signal target across the identity check on modern kernels.
            let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
            let fd = (raw >= 0).then(|| unsafe { OwnedFd::from_raw_fd(raw) });
            let Some((state, parent, _)) = stat(pid).filter(|(_, _, b)| *b == birth) else {
                continue;
            };
            if state == b'Z' {
                if pid == launcher {
                    // ServerProcess reaps its Child with try_wait on the next pass.
                    stopped = false;
                } else if parent == std::process::id() as i32 {
                    if unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) } == 0 {
                        stopped = false;
                    }
                } else if self.0.contains_key(&parent) {
                    // The parent can still be exiting. Wait for adoption before
                    // returning, even if both processes already appear as zombies.
                    stopped = false;
                }
                continue;
            }
            stopped = false;
            unsafe {
                if let Some(fd) = fd {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        fd.as_raw_fd(),
                        libc::SIGKILL,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    );
                } else {
                    libc::kill(pid, libc::SIGKILL);
                }
            }
        }
        stopped
    }
}

pub(super) struct Tracker {
    launcher: i32,
    members: Arc<Mutex<Members>>,
    monitor: tokio::task::JoinHandle<()>,
}

impl Tracker {
    pub(super) fn new(pid: i32) -> Self {
        let mut initial = Members::default();
        if let Some((_, _, birth)) = stat(pid) {
            initial.0.insert(pid, birth);
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
            launcher: pid,
            members,
            monitor,
        }
    }

    pub(super) fn stop(&self) -> bool {
        self.monitor.abort();
        let mut members = self.members.lock().unwrap();
        members.refresh();
        members.kill(self.launcher)
    }
}

/// The dedicated supervisor is a subreaper. When its worker exits, even
/// separate-session orphans become its children. Repeated passes also catch
/// children adopted while their parents are being killed.
pub(super) fn drain_adopted() -> io::Result<bool> {
    let supervisor = std::process::id() as i32;
    for pid in children(supervisor)? {
        // Our direct child's PID is reserved until this supervisor reaps it.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
    // An exiting task can be omitted from procfs' children snapshot. ECHILD
    // also proves the kernel finished reparenting any remaining descendants.
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::process::ServerProcess;

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
        assert!(stat(pid).is_some_and(|(state, parent, _)| state != b'Z' && parent != server.group));
        drop(server);
        assert!(stat(pid).is_none_or(|(state, _, _)| state == b'Z'));
    }
}
