//! Lifetime of a stdio connector, including children of its configured launcher.
use anyhow::Result;
use tokio::process::{Child, Command};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::init_worker_mcp;
#[cfg(target_os = "linux")]
pub(crate) use linux::{drain_predecessors, drain_worker};

pub(super) struct ServerProcess {
    pub child: Child,
    #[cfg(target_os = "linux")]
    tree: linux::Tracker,
    #[cfg(target_os = "linux")]
    stopped: bool,
    #[cfg(all(unix, not(target_os = "linux")))]
    group: i32,
    #[cfg(windows)]
    job: crate::terminal::TerminalJob,
}

impl ServerProcess {
    pub fn spawn(command: &mut Command, _server: &str) -> Result<Self> {
        #[cfg(unix)]
        command.process_group(0);
        #[cfg(windows)]
        command.creation_flags(0x00000004 | 0x00000200); // suspended, new process group
        #[cfg(target_os = "linux")]
        let (child, tree) = linux::spawn(command, _server)?;
        #[cfg(not(target_os = "linux"))]
        let child = command.spawn()?;
        #[cfg(not(target_os = "linux"))]
        let pid = child.id().expect("just spawned");
        #[cfg(windows)]
        let job = {
            let job = crate::terminal::TerminalJob::assign(child.raw_handle().unwrap())?;
            crate::services::tool_hooks::resume_command(pid)?;
            job
        };
        Ok(Self {
            child,
            #[cfg(target_os = "linux")]
            tree,
            #[cfg(target_os = "linux")]
            stopped: false,
            #[cfg(all(unix, not(target_os = "linux")))]
            group: pid as i32,
            #[cfg(windows)]
            job,
        })
    }

    #[cfg(target_os = "linux")]
    pub(super) fn stop(&mut self) -> Result<()> {
        if !self.stopped {
            self.tree.stop(&mut self.child)?;
            self.stopped = true;
        }
        Ok(())
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        #[cfg(windows)]
        if let Err(error) = self.job.terminate_and_wait() {
            tracing::error!(%error, "Terminating MCP server job");
        }
        #[cfg(target_os = "linux")]
        if let Err(error) = self.stop() {
            tracing::error!(%error, "MCP server process tree could not be stopped");
        }
        #[cfg(all(unix, not(target_os = "linux")))]
        {
            unsafe { libc::kill(-self.group, libc::SIGKILL) };
            // Reap the launcher. Its children can briefly remain as zombies in
            // the supervisor (or init) but must no longer be executing at return.
            loop {
                let _ = self.child.try_wait();
                if unsafe { libc::kill(-self.group, 0) } < 0 {
                    break;
                }
                match processes() {
                    Ok(rows) if !rows.iter().any(|&(_, pg, alive)| pg == self.group && alive) => {
                        let _ = self.child.try_wait();
                        while unsafe {
                            libc::waitpid(-self.group, std::ptr::null_mut(), libc::WNOHANG)
                        } > 0
                        {}
                        break;
                    }
                    Err(error) => tracing::warn!(%error, "Checking MCP process group"),
                    _ => {}
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }
}

#[cfg(all(unix, not(target_os = "linux")))]
fn processes() -> Result<Vec<(i32, i32, bool)>> {
    let ps = std::process::Command::new("/bin/ps")
        .args(["-A", "-o", "pid=,pgid=,stat="])
        .output()?;
    anyhow::ensure!(ps.status.success(), "Cannot enumerate process groups");
    let mut rows = Vec::new();
    for line in std::str::from_utf8(&ps.stdout)?.lines() {
        let mut fields = line.split_whitespace();
        if let (Some(pid), Some(pg), Some(state)) = (fields.next(), fields.next(), fields.next()) {
            rows.push((pid.parse()?, pg.parse()?, !state.starts_with('Z')));
        }
    }
    Ok(rows)
}

/// The worker starts a private Unix session. Its groups remain discoverable
/// even if the engine dies between fork and the connector recording its PID.
#[cfg(all(unix, not(target_os = "linux")))]
pub(crate) fn drain_worker_session(session: i32) -> Result<bool> {
    let mut empty = true;
    for (pid, group, alive) in processes()? {
        if unsafe { libc::getsid(pid) } != session {
            continue;
        }
        if alive {
            empty = false;
            unsafe { libc::kill(-group, libc::SIGKILL) };
        }
        unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
    }
    Ok(empty)
}
