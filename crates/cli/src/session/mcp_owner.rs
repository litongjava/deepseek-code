//! MCP operations on the fixed host for a session.
use super::Session;
use anyhow::{bail, Context, Result};
use deepseek_code_core::services::session_worker::{self as worker, McpOp, McpResult};
use deepseek_code_core::{mcp::McpServerManager, runtime::RuntimePaths};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::{Mutex, OnceCell};

const BUSY: &str = "MCP is busy, try after this turn";

impl Session {
    pub(crate) async fn mcp_call(&self, op: McpOp) -> Result<McpResult> {
        if self.worker_frontend {
            let host = worker::ensure_worker(
                &self.paths.data_dir,
                &std::env::current_exe()?,
                &self.id,
                &self.workspace,
            )
            .await?;
            return match self
                .mcp_client
                .lock()
                .await
                .call(&host, &op)
                .await?
                .mcp
                .context("Missing MCP reply")?
            {
                McpResult::Busy => bail!(BUSY),
                result => Ok(result),
            };
        }
        self.ensure_mcp().await?;
        local_operation(&self.paths, &self.workspace, &self.id, &self.mcp, op).await
    }

    pub(crate) async fn ensure_mcp(&self) -> Result<()> {
        self.sdk_cancellable(initialize(&self.mcp, &self.mcp_ready))
            .await
    }

    pub(super) async fn reset_local_mcp(&mut self) -> Result<()> {
        self.mcp.shutdown_all().await?;
        self.mcp_ready = Arc::default();
        Ok(())
    }
}

async fn initialize(manager: &McpServerManager, ready: &OnceCell<()>) -> Result<()> {
    ready
        .get_or_try_init(|| async {
            manager.initialize().await?;
            for server in manager.list_servers().await {
                if server.enabled {
                    if let Err(error) = manager.start_automatically(&server.id).await {
                        crate::compaction::notice(&format!(
                            "MCP: {} failed to start: {error:#}",
                            server.id
                        ));
                    }
                }
            }
            Ok::<_, anyhow::Error>(())
        })
        .await?;
    Ok(())
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

#[derive(Clone)]
pub(crate) struct WorkerMcp {
    pub(super) manager: Arc<McpServerManager>,
    pub(super) ready: Arc<OnceCell<()>>,
    pub(super) gate: Arc<Mutex<()>>,
    paths: RuntimePaths,
    workspace: PathBuf,
    session: String,
}

impl WorkerMcp {
    pub(crate) fn new(paths: RuntimePaths, workspace: PathBuf, session: String) -> Self {
        Self {
            manager: Arc::new(McpServerManager::new(
                paths.data_dir.join("mcp-servers.json"),
            )),
            ready: Arc::default(),
            gate: Arc::default(),
            paths,
            workspace,
            session,
        }
    }

    pub(crate) async fn shutdown(&self) -> Result<()> {
        let _guard = self.gate.lock().await;
        self.manager.shutdown_all().await
    }

    async fn call(&self, op: McpOp) -> Result<McpResult> {
        initialize(&self.manager, &self.ready).await?;
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
impl WorkerMcp {
    /// Take admission before handing the stream to the serving task.
    pub(crate) fn dispatch(
        &self,
        request: worker::Request,
        stream: worker::LocalStream,
        receipts: &worker::McpReceipts,
        busy: bool,
    ) {
        use worker::{Reply, Request};
        let send =
            move |response| tokio::spawn(async move { worker::reply(stream, &response).await });
        let (id, op) = match request {
            Request::Mcp(op) => (None, op),
            Request::McpTracked { id, operation } if operation.needs_receipt() => {
                if let Some(reply) = receipts.admit(id) {
                    send(reply);
                    return;
                }
                (Some(id), operation)
            }
            Request::McpOutcome { id } => {
                send(receipts.query(id));
                return;
            }
            _ => {
                send(Reply::error("Unexpected MCP owner request"));
                return;
            }
        };
        let admission = self.gate.clone().try_lock_owned().ok();
        let target = self.clone();
        let receipts = receipts.clone();
        tokio::spawn(async move {
            let result = if busy {
                Ok(McpResult::Busy)
            } else {
                match admission {
                    Some(_guard) => target.call(op).await,
                    None => Ok(McpResult::Busy),
                }
            };
            let response = match id {
                Some(id) => receipts.finish(id, result),
                None => match result {
                    Ok(mcp) => {
                        let mut reply = Reply::ok(None);
                        reply.mcp = Some(mcp);
                        reply
                    }
                    Err(error) => Reply::error(error),
                },
            };
            send(response);
        });
    }
}
