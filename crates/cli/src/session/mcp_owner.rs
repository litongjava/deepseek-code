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
