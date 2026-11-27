//! Ownership changes are serialized with MCP commands so the old process
//! finishes releasing its children before the new process can start any.
use super::Session;
use anyhow::{ensure, Context, Result};
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
            McpOwner::Frontend => self.mcp.shutdown_all().await?,
            McpOwner::Worker => {
                if let Some(host) = self.running_mcp_worker()? {
                    let reply = worker::call(&host, &Request::Mcp(McpOp::StopAll)).await?;
                    ensure!(
                        matches!(
                            reply.mcp.context("Missing StopAll reply")?,
                            McpResult::Stopped
                        ),
                        "Unexpected StopAll reply"
                    );
                }
            }
            McpOwner::None => {}
        }
        *owner = McpOwner::None;
        if target == McpOwner::Frontend {
            self.mcp.initialize().await?;
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
                return worker::call(&host, &Request::Mcp(op))
                    .await?
                    .mcp
                    .context("Missing MCP reply");
            }
        }
        self.transition_mcp_owner(&mut owner, McpOwner::Frontend)
            .await?;
        local_operation(&self.paths, &self.workspace, &self.id, &self.mcp, op).await
    }
}

async fn reconcile(
    manager: &deepseek_code_core::mcp::McpServerManager,
    paths: &deepseek_code_core::runtime::RuntimePaths,
    session: &str,
) -> Result<()> {
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
    paths: &deepseek_code_core::runtime::RuntimePaths,
    workspace: &std::path::Path,
    session: &str,
    manager: &deepseek_code_core::mcp::McpServerManager,
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
