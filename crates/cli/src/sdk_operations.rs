//! SDK control requests that act on session resources: MCP servers, plugins,
//! background tasks and recorded file checkpoints.
use crate::session::Session;
use anyhow::{bail, ensure, Context, Result};
use deepseek_code_core::{db::message_repo, services::checkpoints};
use serde_json::{json, Value};

pub async fn handle(session: &mut Session, request: &Value) -> Option<Result<Value>> {
    Some(match request["subtype"].as_str()? {
        "mcp_toggle" => mcp_toggle(session, request).await,
        "mcp_reconnect" => mcp_reconnect(session, request).await,
        "mcp_set_servers" => Err(anyhow::anyhow!(
            "mcp_set_servers is not supported; configure servers with `mcp add` or pass sdkMcpServers to initialize"
        )),
        "reload_plugins" => reload_plugins(session).await,
        "stop_task" => stop_task(session, request).await,
        "rewind_files" => rewind_files(session, request),
        "seed_read_state" => seed_read_state(request),
        _ => return None,
    })
}

async fn server_id(session: &Session, request: &Value) -> Result<String> {
    let name = request["serverName"].as_str().context("serverName is required")?;
    session.ensure_mcp().await?;
    ensure!(
        !session.sdk_mcp.names().iter().any(|sdk| sdk == name),
        "SDK-hosted MCP server {name} is managed by the SDK host"
    );
    session
        .mcp
        .list_servers()
        .await
        .into_iter()
        .find(|server| server.name == name || server.id == name)
        .map(|server| server.id)
        .with_context(|| format!("MCP server not found: {name}"))
}

async fn mcp_toggle(session: &Session, request: &Value) -> Result<Value> {
    let enabled = request["enabled"].as_bool().context("enabled must be a boolean")?;
    let id = server_id(session, request).await?;
    session.mcp.set_server_enabled(&id, enabled).await?;
    let running = session.mcp.get_server(&id).await.is_some_and(|server| server.running);
    if enabled && !running {
        session.mcp.start_server(&id).await?;
    } else if !enabled && running {
        session.mcp.stop_server(&id).await?;
    }
    Ok(json!({}))
}

async fn mcp_reconnect(session: &Session, request: &Value) -> Result<Value> {
    let id = server_id(session, request).await?;
    let server = session.mcp.get_server(&id).await.context("MCP server disappeared")?;
    ensure!(server.enabled, "MCP server {} is disabled; enable it with mcp_toggle", server.name);
    session.mcp.restart_server(&id).await?;
    Ok(json!({}))
}

async fn reload_plugins(session: &Session) -> Result<Value> {
    session.reload_plugins().await?;
    let context = crate::services::context(&session.paths)?;
    let plugins = deepseek_code_core::services::plugins::list(&context)?
        .into_iter()
        .filter(|plugin| plugin.enabled)
        .map(|plugin| json!({"name":plugin.id,"path":"","source":plugin.source}))
        .collect::<Vec<_>>();
    let skills = crate::skill_commands::load_for_workspace(&session.paths, &session.workspace).await?;
    let commands = crate::skill_commands::catalog(&skills)
        .into_iter()
        .map(|(name, description)| json!({"name":name.trim_start_matches('/'),"description":description,"argumentHint":""}))
        .collect::<Vec<_>>();
    let agents = deepseek_code_core::services::agents::list(&context)?
        .into_iter()
        .map(|agent| json!({"name":agent.id,"description":agent.description}))
        .collect::<Vec<_>>();
    Ok(json!({"commands":commands,"agents":agents,"plugins":plugins,
        "mcpServers":session.sdk_mcp_status().await,"error_count":0}))
}

async fn stop_task(session: &Session, request: &Value) -> Result<Value> {
    let id = request["task_id"].as_str().context("task_id is required")?;
    let paths = &session.paths;
    if crate::background_commands::managed_shell_records(paths, &session.id)?
        .iter()
        .any(|task| task["id"] == id)
    {
        crate::background_commands::shell_control(paths, &session.id, id, true).await?;
        return Ok(json!({}));
    }
    let job = deepseek_code_core::services::background_sessions::get(&paths.data_dir, id)
        .with_context(|| format!("Task not found: {id}"))?;
    ensure!(job.parent_session_id == session.id, "Task belongs to another session");
    crate::worker_frontend::cancel_job(session, id)?;
    Ok(json!({}))
}

/// The user message persisted most recently; SDK message UUIDs map onto it.
pub fn latest_user_message(session: &Session) -> Result<Option<String>> {
    let messages = session
        .store
        .db
        .with_conn(|conn| message_repo::get_messages_by_conversation(conn, &session.id))??;
    Ok(messages.into_iter().rev().find(|message| message.role == "user").map(|message| message.id))
}

fn line_count(text: Option<&str>) -> u64 {
    text.map_or(0, |text| text.lines().count() as u64)
}

fn rewind_files(session: &Session, request: &Value) -> Result<Value> {
    let requested = request["user_message_id"].as_str().context("user_message_id is required")?;
    let dry_run = request["dry_run"].as_bool().unwrap_or(false);
    let message_id = session
        .sdk_overrides
        .message_ids
        .get(requested)
        .map(String::as_str)
        .unwrap_or(requested);
    let data_dir = &session.paths.data_dir;
    let Some(checkpoint) = checkpoints::list(data_dir, &session.id)?
        .into_iter()
        .find(|checkpoint| checkpoint.message_id == message_id)
    else {
        return Ok(json!({"canRewind":false,"error":"No file checkpoint was recorded for this message (checkpoints require a Git worktree)"}));
    };
    ensure!(
        session.workspace.canonicalize()? == checkpoint.workspace,
        "Checkpoint belongs to a different working directory"
    );
    let preview = checkpoints::preview(data_dir, &session.id, &checkpoint.id)?;
    let (insertions, deletions) = checkpoint.changes.iter().fold((0, 0), |(add, remove), change| {
        // Restoring brings back `before` and removes `after`.
        (add + line_count(change.before.as_deref()), remove + line_count(change.after.as_deref()))
    });
    let blocked = if preview.restored {
        Some("Checkpoint files were already restored".to_owned())
    } else if !preview.conflicts.is_empty() {
        Some(format!("Files changed after the checkpoint: {}", preview.conflicts.join(", ")))
    } else {
        None
    };
    if let Some(error) = blocked {
        return Ok(json!({"canRewind":false,"error":error,"filesChanged":preview.paths}));
    }
    let paths = if dry_run {
        preview.paths
    } else {
        checkpoints::restore(data_dir, &session.id, &checkpoint.id)?.paths
    };
    Ok(json!({"canRewind":true,"filesChanged":paths,"insertions":insertions,"deletions":deletions}))
}

/// File edits here do not depend on a prior Read, so there is no cache to seed.
fn seed_read_state(request: &Value) -> Result<Value> {
    let Some(path) = request["path"].as_str().filter(|path| !path.is_empty()) else {
        bail!("path is required");
    };
    ensure!(request["mtime"].is_number(), "mtime must be a number");
    let _ = path;
    Ok(json!({}))
}
