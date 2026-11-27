//! Requests are delivered to an existing manager, never a temporary replacement process.
use super::McpServerManager;
use crate::db::session_store::McpControl;
use crate::services::{EnvironmentCredentials, ServiceContext};
use anyhow::{ensure, Context, Result};
use serde::Serialize;
use std::{sync::Arc, time::Duration};
fn now() -> i64 {
    chrono::Utc::now().timestamp()
}
#[derive(Debug, Serialize)]
pub struct HostStatus {
    pub id: String,
    pub pid: u32,
    pub servers: Vec<String>,
}
pub struct Host {
    task: Option<tokio::task::JoinHandle<()>>,
    context: ServiceContext,
    id: String,
}
impl Host {
    pub(super) fn start(manager: McpServerManager) -> Result<Self> {
        let root = manager
            .config_path
            .parent()
            .context("MCP config has no parent")?;
        let context = ServiceContext::open(
            crate::runtime::RuntimePaths::new(root.to_path_buf())?,
            Arc::new(EnvironmentCredentials),
        )?;
        let id = uuid::Uuid::new_v4().to_string();
        let worker_context = context.clone();
        let worker_id = id.clone();
        let task = tokio::spawn(async move {
            if let Err(error) = serve(&worker_context, &worker_id, &manager).await {
                tracing::error!("MCP control host failed: {error}");
            }
        });
        Ok(Self {
            task: Some(task),
            context,
            id,
        })
    }
    pub async fn stop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
    }
}
impl Drop for Host {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        let _ = self.context.db.remove_mcp_host(&self.id);
    }
}
async fn serve(
    context: &ServiceContext<impl McpControl>,
    id: &str,
    manager: &McpServerManager,
) -> Result<()> {
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let mut last_heartbeat: Option<tokio::time::Instant> = None;
    let mut last_servers_json = String::new();
    // A restart result whose write-back failed while the database was busy.
    let mut pending: Option<(String, Option<String>)> = None;
    let mut last_warning: Option<tokio::time::Instant> = None;
    loop {
        tick.tick().await;
        let result = tick_once(
            context,
            id,
            manager,
            &mut last_heartbeat,
            &mut last_servers_json,
            &mut pending,
        )
        .await;
        if let Err(error) = result {
            // Database contention is expected; keep serving and avoid log floods.
            if last_warning.is_none_or(|at| at.elapsed() >= Duration::from_secs(60)) {
                tracing::warn!("MCP control host tick failed: {error}");
                last_warning = Some(tokio::time::Instant::now());
            }
        }
    }
}

fn write_request_result(
    context: &ServiceContext<impl McpControl>,
    request: &str,
    error: &Option<String>,
) -> Result<usize> {
    context.db.report_mcp_restart(request, error.as_deref())
}

async fn tick_once(
    context: &ServiceContext<impl McpControl>,
    id: &str,
    manager: &McpServerManager,
    last_heartbeat: &mut Option<tokio::time::Instant>,
    last_servers_json: &mut String,
    pending: &mut Option<(String, Option<String>)>,
) -> Result<()> {
    let servers: Vec<_> = manager
        .list_servers()
        .await
        .into_iter()
        .filter(|s| s.running)
        .map(|s| s.id)
        .collect();
    let servers_json = serde_json::to_string(&servers)?;
    // The row stays valid for 30 s; rewriting it every 5 s is enough.
    let heartbeat_due = *last_servers_json != servers_json
        || last_heartbeat.is_none_or(|at| at.elapsed() >= Duration::from_secs(5));
    if heartbeat_due {
        context
            .db
            .heartbeat_mcp(id, std::process::id(), now() + 30, &servers_json)?;
        *last_heartbeat = Some(tokio::time::Instant::now());
        *last_servers_json = servers_json;
    }
    if let Some((request, error)) = pending.take() {
        if let Err(write_error) = write_request_result(context, &request, &error) {
            *pending = Some((request, error));
            return Err(write_error);
        }
    }
    if let Some((request, server)) = context.db.claim_mcp_restart(id, now())? {
        let result = if servers.contains(&server) {
            if let Some(endpoint) = &manager.worker_endpoint {
                use crate::services::session_worker::{mcp_operations, McpOp, McpResult};
                let operation = McpOp::Reconnect { server };
                let reply =
                    mcp_operations::tracked(endpoint, uuid::Uuid::parse_str(&request)?, &operation)
                        .await;
                match reply {
                    Ok(reply) if !reply.ok => {
                        Err(anyhow::anyhow!("{}", reply.error.unwrap_or_default()))
                    }
                    Ok(reply) if matches!(reply.mcp, Some(McpResult::Busy)) => {
                        Err(anyhow::anyhow!("MCP is busy, try after this turn"))
                    }
                    Ok(reply) if matches!(reply.mcp, Some(McpResult::Message { .. })) => Ok(()),
                    Ok(_) => Err(anyhow::anyhow!("Unexpected worker restart reply")),
                    Err(error) => Err(error),
                }
            } else {
                manager.restart_server(&server).await
            }
        } else {
            Err(anyhow::anyhow!("Server is no longer running in this host"))
        };
        let error = result.err().map(|e| e.to_string());
        if let Err(write_error) = write_request_result(context, &request, &error) {
            *pending = Some((request, error));
            return Err(write_error);
        }
    }
    Ok(())
}
pub fn hosts(context: &ServiceContext<impl McpControl>) -> Result<Vec<HostStatus>> {
    context
        .db
        .mcp_advertisements(now())?
        .into_iter()
        .map(|(id, pid, servers)| {
            Ok(HostStatus {
                id,
                pid,
                servers: serde_json::from_str(&servers)?,
            })
        })
        .collect()
}
pub async fn restart(
    context: &ServiceContext<impl McpControl>,
    server: &str,
    host: Option<&str>,
) -> Result<String> {
    restart_from_manager(context, server, host, None).await
}

pub async fn restart_from_manager(
    context: &ServiceContext<impl McpControl>,
    server: &str,
    host: Option<&str>,
    admitted: Option<&McpServerManager>,
) -> Result<String> {
    let matches: Vec<_> = hosts(context)?
        .into_iter()
        .filter(|h| host.is_none_or(|id| id == h.id) && h.servers.iter().any(|s| s == server))
        .collect();
    ensure!(!matches.is_empty(), "No running host owns this MCP server");
    ensure!(
        matches.len() == 1,
        "Multiple hosts own this server; select --host from mcp hosts"
    );
    let id = uuid::Uuid::new_v4().to_string();
    let host = &matches[0].id;
    if let Some(manager) = admitted {
        let local = manager
            .control
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|h| h.id == *host);
        if local {
            manager.restart_server(server).await?;
            return Ok(host.clone());
        }
    }
    context
        .db
        .request_mcp_restart(&id, host, server, now() + 25)?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(27);
    loop {
        let (state, error) = context.db.mcp_restart_status(&id)?;
        if state == "completed" {
            return Ok(host.clone());
        }
        if state == "failed" {
            anyhow::bail!("MCP restart failed: {}", error.unwrap_or_default());
        }
        ensure!(
            tokio::time::Instant::now() < deadline
                || (state == "running"
                    && !crate::services::session_worker::process_exited(matches[0].pid)),
            "MCP host did not acknowledge restart request {id} (last state: {state})"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    type ServiceContext = crate::services::ServiceContext<crate::db::session_store::Stores>;
    use crate::runtime::RuntimePaths;

    fn fixture() -> (tempfile::TempDir, ServiceContext) {
        let temp = tempfile::tempdir().unwrap();
        let context = ServiceContext::from_stores(
            RuntimePaths::new(temp.path().to_path_buf()).unwrap(),
            Arc::new(EnvironmentCredentials),
        )
        .unwrap();
        (temp, context)
    }

    fn host_row(context: &ServiceContext, id: &str) -> Option<(i64, String)> {
        context.db.mcp_advertisement(id).unwrap()
    }

    async fn wait_for_host_row(context: &ServiceContext, id: &str) -> (i64, String) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(row) = host_row(context, id) {
                return row;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "host row was not written"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    fn spawn_host(
        context: &ServiceContext,
        manager: McpServerManager,
        id: &str,
    ) -> tokio::task::JoinHandle<Result<()>> {
        let context = context.clone();
        let id = id.to_owned();
        tokio::spawn(async move { serve(&context, &id, &manager).await })
    }

    #[tokio::test]
    async fn heartbeat_is_not_rewritten_every_tick() {
        let (temp, context) = fixture();
        let manager = McpServerManager::new(temp.path().join("mcp.json"));
        let id = uuid::Uuid::new_v4().to_string();
        let task = spawn_host(&context, manager, &id);
        let first = wait_for_host_row(&context, &id).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let early = host_row(&context, &id).unwrap();
        tokio::time::sleep(Duration::from_millis(1200)).await;
        let late = host_row(&context, &id).unwrap();
        task.abort();
        let _ = task.await;
        assert_eq!(first, early);
        assert_eq!(
            early, late,
            "heartbeat was rewritten although nothing changed"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn serve_survives_a_locked_database() {
        let (temp, context) = fixture();
        let manager = McpServerManager::new(temp.path().join("mcp.json"));
        let id = uuid::Uuid::new_v4().to_string();
        let task = spawn_host(&context, manager, &id);
        let first = wait_for_host_row(&context, &id).await;

        let path = temp.path().join("global.db");
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let conn = rusqlite::Connection::open(path).unwrap();
            conn.busy_timeout(Duration::from_secs(10)).unwrap();
            conn.execute_batch("BEGIN EXCLUSIVE").unwrap();
            locked_tx.send(()).unwrap();
            // Longer than the pool's 5 s busy timeout, so several host writes fail.
            std::thread::sleep(Duration::from_secs(12));
            conn.execute_batch("COMMIT").unwrap();
        });
        locked_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        tokio::time::sleep(Duration::from_secs(13)).await;
        holder.join().unwrap();

        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            if host_row(&context, &id).is_some_and(|row| row.0 > first.0) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "host row was not refreshed after the lock was released"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        let request = uuid::Uuid::new_v4().to_string();
        context
            .db
            .request_mcp_restart(&request, &id, "missing", now() + 25)
            .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let state = loop {
            let (state, _) = context.db.mcp_restart_status(&request).unwrap();
            if state != "pending" {
                break state;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "restart request was left pending after the lock was released"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        task.abort();
        let _ = task.await;
        assert!(
            state == "completed" || state == "failed",
            "unexpected state {state}"
        );
    }
}
