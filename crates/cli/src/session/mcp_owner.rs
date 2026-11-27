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
    pub(crate) fn uses_worker(&self) -> bool {
        self.mcp.is_none()
    }

    pub(crate) fn local_mcp(&self) -> Result<&McpServerManager> {
        self.mcp
            .as_deref()
            .context("This session runs MCP in its worker")
    }

    pub(crate) async fn mcp_call(&self, op: McpOp) -> Result<McpResult> {
        if self.uses_worker() {
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
        local_operation(&self.paths, &self.workspace, self.local_mcp()?, op).await
    }

    pub(crate) async fn ensure_mcp(&self) -> Result<()> {
        self.sdk_cancellable(initialize(self.local_mcp()?, &self.mcp_ready))
            .await
    }

    pub(super) async fn reset_local_mcp(&mut self) -> Result<()> {
        if let Some(manager) = &self.mcp {
            manager.shutdown_all().await?;
        }
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
async fn reconcile(manager: &McpServerManager) {
    match manager.reconcile_config().await {
        Ok(changes) => {
            for (id, action) in changes {
                crate::compaction::notice(&format!("MCP: {id} {action}"));
            }
        }
        Err(err) => crate::compaction::notice(&format!("MCP reload: {err:#}")),
    }
}

pub(crate) async fn local_operation(
    paths: &RuntimePaths,
    workspace: &Path,
    manager: &McpServerManager,
    op: McpOp,
) -> Result<McpResult> {
    reconcile(manager).await;
    if matches!(op, McpOp::ReloadPlugins | McpOp::SetPluginEnabled { .. }) {
        let context = crate::services::context(paths)?;
        let notices = if let McpOp::SetPluginEnabled { id, enabled } = op {
            deepseek_code_core::services::plugins::set_mcp_enabled(&context, manager, &id, enabled)
                .await?
        } else {
            deepseek_code_core::services::plugins::sync_mcp(&context, manager).await?
        };
        reconcile(manager).await;
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
}

impl WorkerMcp {
    pub(crate) fn new(paths: RuntimePaths, workspace: PathBuf, endpoint: String) -> Self {
        Self {
            manager: Arc::new(
                McpServerManager::new(paths.data_dir.join("mcp-servers.json"))
                    .with_worker_endpoint(endpoint),
            ),
            ready: Arc::default(),
            gate: Arc::default(),
            paths,
            workspace,
        }
    }

    pub(crate) fn runtime_status(
        &self,
    ) -> deepseek_code_core::services::runtime_status::RuntimeSource {
        deepseek_code_core::services::runtime_status::RuntimeSource {
            mcp: Some(self.manager.clone()),
            mcp_ready: self.ready.clone(),
            data_dir: self.paths.data_dir.clone(),
            workspace: self.workspace.clone(),
        }
    }

    pub(crate) async fn shutdown(&self) -> Result<()> {
        let _guard = self.gate.lock().await;
        self.manager.shutdown_all().await
    }

    async fn call(&self, op: McpOp) -> Result<McpResult> {
        initialize(&self.manager, &self.ready).await?;
        local_operation(&self.paths, &self.workspace, &self.manager, op).await
    }

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
            Request::Mcp(McpOp::ListPlugins) => {
                let result = crate::services::context(&self.paths).and_then(|ctx| {
                    Ok(McpResult::Json {
                        value: serde_json::to_value(deepseek_code_core::services::plugins::list(
                            &ctx,
                        )?)?,
                    })
                });
                send(worker::mcp_reply(result));
                return;
            }
            Request::McpTracked { id, operation } if operation.needs_receipt() => {
                if let Some(reply) = receipts.admit(id) {
                    send(reply);
                    return;
                }
                (id, operation)
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
            let response = receipts.finish(id, result);
            send(response);
        });
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use deepseek_code_core::mcp::McpServerConfig;
    use std::{fs, time::Duration};
    use tokio::io::{AsyncBufReadExt, BufReader};

    #[tokio::test]
    #[ignore = "requires a seccomp runner denying pidfd_open and numeric TERM/KILL"]
    async fn refused_signals_reach_disable_enable_and_remove_replies() {
        let root = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(root.path().join("data")).unwrap();
        let target = WorkerMcp::new(paths, root.path().into(), "test-worker".into());
        let starts = root.path().join("starts");
        let keep = root.path().join("keep");
        fs::write(&keep, "").unwrap();
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../core/tests/fixtures/live-mcp.cjs");
        let script = r#"const fs = require('fs');
            fs.appendFileSync(process.argv[2], process.pid + '\n');
            setInterval(() => { if (!fs.existsSync(process.argv[3])) process.exit(0) }, 20);
            require(process.argv[1]);"#;
        target
            .manager
            .add_server(McpServerConfig {
                id: "w".into(),
                name: "w".into(),
                command: "node".into(),
                enabled: true,
                args: vec![
                    "-e".into(),
                    script.into(),
                    fixture.display().to_string(),
                    starts.display().to_string(),
                    keep.display().to_string(),
                ],
                ..Default::default()
            })
            .await
            .unwrap();
        target.manager.start_server("w").await.unwrap();
        // Ownership is already initialized. Exercise the ordinary worker
        // dispatcher and serialized receipts without binding a listener.
        target.ready.set(()).unwrap();
        let original = fs::read_to_string(&starts).unwrap();
        let receipts = worker::McpReceipts::default();
        for operation in [
            McpOp::State {
                server: Some("w".into()),
                state: Some("off".into()),
            },
            McpOp::State {
                server: Some("w".into()),
                state: Some("user".into()),
            },
            McpOp::Remove { server: "w".into() },
        ] {
            let request = worker::Request::McpTracked {
                id: uuid::Uuid::new_v4(),
                operation,
            };
            let (client, server) = tokio::io::duplex(4096);
            let before = std::time::Instant::now();
            target.dispatch(request, Box::new(server), &receipts, false);
            let mut line = String::new();
            tokio::time::timeout(
                Duration::from_secs(5),
                BufReader::new(client).read_line(&mut line),
            )
            .await
            .unwrap()
            .unwrap();
            eprintln!("{:.2?} {}", before.elapsed(), line.trim_end());
            let reply: worker::Reply = serde_json::from_str(&line).unwrap();
            assert!(!reply.ok && reply.mcp.is_none());
            let error = reply.error.unwrap();
            assert!(
                error.contains("MCP server w could not be stopped")
                    && error.contains(original.trim())
                    && error.contains("os error 1"),
                "{error}"
            );
        }
        assert_eq!(fs::read_to_string(&starts).unwrap(), original);
        fs::remove_file(keep).unwrap();
        target.manager.stop_server("w").await.unwrap();
    }
}
