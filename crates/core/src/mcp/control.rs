//! Requests are delivered to an existing manager, never a temporary replacement process.
use super::McpServerManager;
use crate::services::{EnvironmentCredentials, ServiceContext};
use anyhow::{ensure, Context, Result};
use rusqlite::{params, OptionalExtension};
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
        let _ = self
            .context
            .db
            .with_conn(|c| c.execute("DELETE FROM mcp_hosts WHERE id=?1", [&self.id]));
    }
}
async fn serve(context: &ServiceContext, id: &str, manager: &McpServerManager) -> Result<()> {
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
    context: &ServiceContext,
    request: &str,
    error: &Option<String>,
) -> Result<usize> {
    Ok(context.db.with_conn(|c| {
        c.execute(
            "UPDATE mcp_requests SET state=?1,error=?2 WHERE id=?3",
            params![
                if error.is_some() {
                    "failed"
                } else {
                    "completed"
                },
                error,
                request
            ],
        )
    })??)
}

async fn tick_once(
    context: &ServiceContext,
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
        context.db.with_conn(|c|c.execute("INSERT INTO mcp_hosts(id,pid,expires_at,servers) VALUES(?1,?2,?3,?4) ON CONFLICT(id) DO UPDATE SET expires_at=excluded.expires_at,servers=excluded.servers",params![id,std::process::id(),now()+30,servers_json]))??;
        *last_heartbeat = Some(tokio::time::Instant::now());
        *last_servers_json = servers_json;
    }
    if let Some((request, error)) = pending.take() {
        if let Err(write_error) = write_request_result(context, &request, &error) {
            *pending = Some((request, error));
            return Err(write_error);
        }
    }
    let request:Option<(String,String)>=context.db.with_conn(|c|c.query_row("SELECT id,server FROM mcp_requests WHERE host=?1 AND state='pending' AND expires_at>?2 ORDER BY rowid LIMIT 1",params![id,now()],|r|Ok((r.get(0)?,r.get(1)?))).optional())??;
    if let Some((request, server)) = request {
        let claimed=context.db.with_conn(|c|c.execute("UPDATE mcp_requests SET state='running' WHERE id=?1 AND state='pending' AND expires_at>?2",params![request,now()]))??;
        if claimed != 1 {
            return Ok(());
        }
        let result = if servers.contains(&server) {
            match tokio::time::timeout(Duration::from_secs(20), manager.restart_server(&server))
                .await
            {
                Ok(result) => result,
                Err(_) => Err(anyhow::anyhow!("MCP restart timed out")),
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
pub fn hosts(context: &ServiceContext) -> Result<Vec<HostStatus>> {
    context.db.with_conn(|c| -> Result<_> {
        let mut s =
            c.prepare("SELECT id,pid,servers FROM mcp_hosts WHERE expires_at>?1 ORDER BY id")?;
        let rows = s.query_map([now()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, u32>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        rows.map(|row| {
            let (id, pid, servers) = row?;
            Ok(HostStatus {
                id,
                pid,
                servers: serde_json::from_str(&servers)?,
            })
        })
        .collect()
    })?
}
pub async fn restart(context: &ServiceContext, server: &str, host: Option<&str>) -> Result<String> {
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
    context.db.with_conn(|c|c.execute("INSERT INTO mcp_requests(id,host,server,expires_at,state) VALUES(?1,?2,?3,?4,'pending')",params![id,host,server,now()+25]))??;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(27);
    loop {
        let (state, error): (String, Option<String>) = context.db.with_conn(|c| {
            c.query_row(
                "SELECT state,error FROM mcp_requests WHERE id=?1",
                [&id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        })??;
        if state == "completed" {
            return Ok(host.clone());
        }
        if state == "failed" {
            anyhow::bail!("MCP restart failed: {}", error.unwrap_or_default());
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "MCP host did not acknowledge restart; check host status"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::RuntimePaths;

    fn fixture() -> (tempfile::TempDir, ServiceContext) {
        let temp = tempfile::tempdir().unwrap();
        let context = ServiceContext::open(
            RuntimePaths::new(temp.path().to_path_buf()).unwrap(),
            Arc::new(EnvironmentCredentials),
        )
        .unwrap();
        (temp, context)
    }

    fn host_row(context: &ServiceContext, id: &str) -> Option<(i64, String)> {
        context
            .db
            .with_conn(|c| {
                c.query_row(
                    "SELECT expires_at,servers FROM mcp_hosts WHERE id=?1",
                    [id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
            })
            .unwrap()
            .unwrap()
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

        let path = temp.path().join(crate::runtime::DATABASE_FILE);
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
            .with_conn(|c| {
                c.execute(
                    "INSERT INTO mcp_requests(id,host,server,expires_at,state) VALUES(?1,?2,?3,?4,'pending')",
                    params![request, id, "missing", now() + 25],
                )
            })
            .unwrap()
            .unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        let state = loop {
            let state: String = context
                .db
                .with_conn(|c| {
                    c.query_row(
                        "SELECT state FROM mcp_requests WHERE id=?1",
                        [&request],
                        |r| r.get(0),
                    )
                })
                .unwrap()
                .unwrap();
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
