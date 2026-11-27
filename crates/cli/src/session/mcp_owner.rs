//! Ownership changes are serialized with MCP commands so the old process
//! finishes releasing its children before the new process can start any.
use super::Session;
use anyhow::{bail, Context, Result};
use deepseek_code_core::services::session_worker::{self as worker, McpOp, McpResult};
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
    let dir = paths.layout().lock_path("mcp-owner");
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum McpOwner {
    None,
    Frontend,
    Worker,
}

impl Session {
    async fn running_mcp_worker(&self) -> Result<Option<worker::Worker>> {
        worker::live_mcp_worker(&self.paths.data_dir, &self.id).await
    }

    /// An explicit target hands off a turn. A command passes None to retain
    /// the current owner, starting locally if the session has no owner yet.
    /// Keep the guard through dispatch so a handoff cannot overtake the call.
    pub(crate) async fn set_mcp_owner(
        &self,
        target: impl Into<Option<McpOwner>>,
    ) -> Result<tokio::sync::MutexGuard<'_, McpOwner>> {
        let mut owner = self.mcp_owner.lock().await;
        self.discover_mcp_owner(&mut owner).await?;
        let target = target.into().unwrap_or_else(|| match *owner {
            McpOwner::None => McpOwner::Frontend,
            current => current,
        });
        self.transition_mcp_owner(&mut owner, target).await?;
        Ok(owner)
    }

    async fn transition_mcp_owner(&self, owner: &mut McpOwner, target: McpOwner) -> Result<()> {
        if *owner == target {
            return Ok(());
        }
        match *owner {
            McpOwner::Frontend => release_local(&self.mcp, &self.mcp_lease, owner).await?,
            McpOwner::Worker => {
                if let Some(host) = self.running_mcp_worker().await? {
                    let reply = self
                        .mcp_client
                        .lock()
                        .await
                        .call(&host, &McpOp::StopAll)
                        .await?;
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
                || deepseek_code_core::services::background_sessions::worker_directory(
                    &self.paths.data_dir,
                    &self.id,
                )?
                .is_some()
            {
                Some(acquire_lease(&self.paths, &self.id)?)
            } else {
                None
            };
            self.mcp.initialize().await?;
            *self.mcp_lease.lock().unwrap() = lease;
            let _ = self.mcp_ready.set(());
            *owner = target;
            let startup = async {
                for server in self.mcp.list_servers().await {
                    if server.enabled {
                        if let Err(error) = self.mcp.start_automatically(&server.id).await {
                            crate::compaction::notice(&format!(
                                "MCP: {} failed to start: {error:#}",
                                server.id
                            ));
                        }
                    }
                }
                Ok(())
            };
            if let Err(error) = self.sdk_cancellable(startup).await {
                release_local(&self.mcp, &self.mcp_lease, owner).await?;
                return Err(error);
            }
        } else {
            *owner = target;
        }
        Ok(())
    }

    async fn discover_mcp_owner(&self, owner: &mut McpOwner) -> Result<()> {
        if *owner == McpOwner::None
            && self.observe_mcp_worker
            && self.running_mcp_worker().await?.is_some()
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
        if matches!(op, McpOp::StopAll) {
            drop(self.set_mcp_owner(McpOwner::None).await?);
            return Ok(McpResult::Stopped);
        }
        let mut owner = self.set_mcp_owner(None).await?;
        if *owner == McpOwner::Worker {
            if let Some(host) = self.running_mcp_worker().await? {
                match self
                    .mcp_client
                    .lock()
                    .await
                    .call(&host, &op)
                    .await?
                    .mcp
                    .context("Missing MCP reply")?
                {
                    McpResult::Busy => bail!(BUSY),
                    McpResult::Unavailable => *owner = McpOwner::None,
                    result => return Ok(result),
                }
            }
            self.transition_mcp_owner(&mut owner, McpOwner::Frontend)
                .await?;
        }
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
    if matches!(op, McpOp::ReloadPlugins | McpOp::SetPluginEnabled { .. }) {
        let context = crate::services::context(paths)?;
        if let McpOp::SetPluginEnabled { id, enabled } = op {
            deepseek_code_core::services::plugins::enable(&context, &id, enabled)?;
        }
        let notices = deepseek_code_core::services::plugins::sync_mcp(&context, manager).await?;
        reconcile(manager, paths, session).await?;
        reload_plugin_metadata(paths, workspace)?;
        return Ok(McpResult::Reloaded { notices });
    }
    crate::mcp_commands::execute(paths, manager, op, true).await
}

pub(super) fn reload_plugin_metadata(paths: &RuntimePaths, workspace: &Path) -> Result<()> {
    let context = crate::services::context(paths)?;
    deepseek_code_core::tools::configure_lsp_servers(
        &workspace.to_string_lossy(),
        deepseek_code_core::services::plugins::all_language_servers(&context)?,
    )?;
    Ok(())
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
        if matches!(op, McpOp::ProbeOwner) {
            return Ok(McpResult::Owned);
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
