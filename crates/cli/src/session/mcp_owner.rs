//! Ownership changes are serialized with MCP commands so the old process
//! finishes releasing its children before the new process can start any.
use super::Session;
use anyhow::{bail, Context, Result};
use deepseek_code_core::{fs::OwnedFileLock, mcp::McpServerManager, runtime::RuntimePaths};
use std::{
    fs::File,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::Mutex;

const BUSY: &str = "MCP is busy, try after this turn";

fn acquire_lease(paths: &RuntimePaths, session: &str) -> Result<OwnedFileLock> {
    let id = uuid::Uuid::parse_str(session)?;
    let dir = paths.layout().state_dir("locks/mcp-owner");
    std::fs::create_dir_all(&dir)?;
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(format!("{id}.lock")))?;
    OwnedFileLock::try_acquire(file)?.context("Another process owns MCP for this session")
}

type Lease = Arc<std::sync::Mutex<Option<OwnedFileLock>>>;

async fn release_local(
    manager: &McpServerManager,
    lease: &Lease,
    owner: &mut McpOwner,
) -> Result<()> {
    manager.shutdown_all().await?;
    lease.lock().unwrap().take();
    *owner = McpOwner::None;
    Ok(())
}

use deepseek_code_core::services::session_worker::{self as worker, McpOp, McpResult, Request};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum McpOwner {
    None,
    Frontend,
    Worker,
}

impl Session {
    pub(crate) fn running_mcp_worker(&self) -> Result<Option<worker::Worker>> {
        Ok(
            worker::current(&self.paths.data_dir, &self.id)?.filter(|host| {
                host.status == "running" && !host.child_pid.is_some_and(worker::process_exited)
            }),
        )
    }

    pub(crate) async fn set_mcp_owner(&self, target: McpOwner) -> Result<()> {
        let mut owner = self.mcp_owner.lock().await;
        self.discover_mcp_owner(&mut owner)?;
        self.transition_mcp_owner(&mut owner, target).await
    }

    pub(super) async fn transition_mcp_owner(
        &self,
        owner: &mut McpOwner,
        target: McpOwner,
    ) -> Result<()> {
        if *owner == target {
            return Ok(());
        }
        match *owner {
            McpOwner::Frontend => release_local(&self.mcp, &self.mcp_lease, owner).await?,
            McpOwner::Worker => {
                if let Some(host) = self.running_mcp_worker()? {
                    let reply = worker::call(&host, &Request::Mcp(McpOp::StopAll)).await?;
                    match reply.mcp.context("Missing StopAll reply")? {
                        McpResult::Stopped => {}
                        McpResult::Busy => bail!(BUSY),
                        _ => bail!("Unexpected StopAll reply"),
                    }
                }
            }
            McpOwner::None => {}
        }
        *owner = McpOwner::None;
        if target == McpOwner::Frontend {
            let lease = if self.observe_mcp_worker
                || self.coordination_mode == crate::local_coordination::RegistrationMode::HostOwned
            {
                Some(acquire_lease(&self.paths, &self.id)?)
            } else {
                None
            };
            self.mcp.initialize().await?;
            *self.mcp_lease.lock().unwrap() = lease;
            let _ = self.mcp_ready.set(());
            *owner = target;
            for server in self.mcp.list_servers().await {
                if server.enabled {
                    if let Err(error) = self.mcp.start_server(&server.id).await {
                        crate::compaction::notice(&format!(
                            "MCP: {} failed to start: {error:#}",
                            server.id
                        ));
                    }
                }
            }
        } else {
            *owner = target;
        }
        Ok(())
    }
}

impl Session {
    fn discover_mcp_owner(&self, owner: &mut McpOwner) -> Result<()> {
        if *owner == McpOwner::None
            && self.observe_mcp_worker
            && self.running_mcp_worker()?.is_some()
        {
            *owner = McpOwner::Worker;
        }
        Ok(())
    }

    // Leaving a conversation releases only this process's manager. A resident
    // worker can continue its admitted work after the frontend detaches.
    pub(super) async fn detach_mcp_owner(&self) -> Result<()> {
        let mut owner = self.mcp_owner.lock().await;
        if *owner == McpOwner::Frontend {
            self.transition_mcp_owner(&mut owner, McpOwner::None)
                .await?;
        }
        *owner = McpOwner::None;
        Ok(())
    }

    pub(crate) async fn mcp_call(&self, op: McpOp) -> Result<McpResult> {
        let mut owner = self.mcp_owner.lock().await;
        if matches!(op, McpOp::StopAll) {
            self.transition_mcp_owner(&mut owner, McpOwner::None)
                .await?;
            return Ok(McpResult::Stopped);
        }
        self.discover_mcp_owner(&mut owner)?;
        if *owner == McpOwner::Worker {
            if let Some(host) = self.running_mcp_worker()? {
                match worker::call(&host, &Request::Mcp(op.clone()))
                    .await?
                    .mcp
                    .context("Missing MCP reply")?
                {
                    McpResult::Busy => bail!(BUSY),
                    McpResult::Unavailable => *owner = McpOwner::None,
                    result => return Ok(result),
                }
            }
        }
        self.transition_mcp_owner(&mut owner, McpOwner::Frontend)
            .await?;
        local_operation(&self.paths, &self.workspace, &self.id, &self.mcp, op).await
    }
}

async fn reconcile(manager: &McpServerManager, paths: &RuntimePaths, session: &str) -> Result<()> {
    match manager.reconcile_config().await {
        Ok(changes) => {
            deepseek_code_core::mcp::invalidate_snapshots(&paths.data_dir, session, &changes)?;
            for (id, action) in changes {
                crate::compaction::notice(&format!("MCP: {id} {action}"));
            }
        }
        Err(err) => crate::compaction::notice(&format!("MCP reload: {err:#}")),
    }
    Ok(())
}

pub(crate) async fn local_operation(
    paths: &RuntimePaths,
    workspace: &Path,
    session: &str,
    manager: &McpServerManager,
    op: McpOp,
) -> Result<McpResult> {
    reconcile(manager, paths, session).await?;
    if matches!(op, McpOp::ReloadPlugins) {
        let context = crate::services::context(paths)?;
        let notices = deepseek_code_core::services::plugins::sync_mcp(&context, manager).await?;
        reconcile(manager, paths, session).await?;
        deepseek_code_core::tools::configure_lsp_servers(
            &workspace.to_string_lossy(),
            deepseek_code_core::services::plugins::all_language_servers(&context)?,
        )?;
        return Ok(McpResult::Reloaded { notices });
    }
    crate::mcp_commands::execute(paths, manager, op, true).await
}

/// A handle to the worker Session's actual manager, published before a turn.
/// Ownership and manager operations share the same gate as the local turn.
#[derive(Clone)]
pub(crate) struct WorkerMcp {
    manager: Arc<McpServerManager>,
    owner: Arc<Mutex<McpOwner>>,
    lease: Lease,
    paths: RuntimePaths,
    workspace: PathBuf,
    session: String,
}

impl Session {
    pub(crate) fn worker_mcp(&self) -> WorkerMcp {
        WorkerMcp {
            manager: self.mcp.clone(),
            owner: self.mcp_owner.clone(),
            lease: self.mcp_lease.clone(),
            paths: self.paths.clone(),
            workspace: self.workspace.clone(),
            session: self.id.clone(),
        }
    }

    pub(crate) fn mcp_released(&self) -> bool {
        self.mcp_owner
            .try_lock()
            .map_or(true, |owner| *owner == McpOwner::None)
    }
}

impl WorkerMcp {
    pub(crate) fn try_admit(&self) -> Option<tokio::sync::OwnedMutexGuard<McpOwner>> {
        self.owner.clone().try_lock_owned().ok()
    }

    pub(crate) async fn call(
        &self,
        mut owner: tokio::sync::OwnedMutexGuard<McpOwner>,
        op: McpOp,
    ) -> Result<McpResult> {
        if matches!(op, McpOp::StopAll) {
            if *owner == McpOwner::Frontend {
                release_local(&self.manager, &self.lease, &mut owner).await?;
            }
            return Ok(McpResult::Stopped);
        }
        if *owner != McpOwner::Frontend {
            return Ok(McpResult::Unavailable);
        }
        local_operation(
            &self.paths,
            &self.workspace,
            &self.session,
            &self.manager,
            op,
        )
        .await
    }
}
