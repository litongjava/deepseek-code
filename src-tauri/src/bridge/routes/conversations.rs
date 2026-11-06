use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{delete, get, post},
    Json, Router,
};
use serde::{Deserialize};
use serde_json;

use super::super::AppState;
use crate::engine::EnginePool;
use crate::native_engine::NativeEngine;
use crate::permissions::PermissionMode;

#[derive(Deserialize)]
struct CreateConversationBody {
    title: Option<String>,
    model: Option<String>,
    research_mode: Option<bool>,
    #[serde(default)]
    workspace_mode: crate::db::conversation_repo::WorkspaceMode,
}

async fn conversations_list(State(state): State<AppState>) -> Json<serde_json::Value> {
    let db = state.db_manager.clone();
    let result = tokio::task::spawn_blocking(move || {
        db.with_conn(|conn| crate::db::conversation_repo::list_conversations(conn))
    }).await;
    match result {
        Ok(Ok(Ok(convs))) => Json(serde_json::json!({ "conversations": convs })),
        _ => Json(serde_json::json!({ "conversations": [] })),
    }
}

async fn conversations_create(State(state): State<AppState>, Json(body): Json<CreateConversationBody>) -> Result<Json<serde_json::Value>, StatusCode> {
    let id = uuid::Uuid::new_v4().to_string();
    let db = state.db_manager.clone();
    let id_clone = id.clone();
    let title_clone = body.title.clone();
    let model_clone = body.model.clone();
    let workspace_mode = body.workspace_mode.as_str();
    let result = tokio::task::spawn_blocking(move || {
        db.with_conn(|conn| {
            let tx = deepseek_code_core::db::write_transaction(&conn)?;
            let now = chrono::Utc::now().to_rfc3339();
            crate::db::conversation_repo::insert_conversation(
                &tx, &id_clone,
                title_clone.as_deref(),
                model_clone.as_deref(),
                None, None, None,
                body.research_mode.unwrap_or(false), false, false,
                &now, &now, 0
            )?;
            crate::db::conversation_repo::set_workspace_mode(&tx, &id_clone, workspace_mode)?;
            tx.commit()?;
            Ok::<(), anyhow::Error>(())
        })
    }).await;
    let now = chrono::Utc::now().to_rfc3339();
    if !matches!(result, Ok(Ok(Ok(())))) { return Err(StatusCode::INTERNAL_SERVER_ERROR); }
    Ok(Json(serde_json::json!({
        "id": id,
        "title": body.title,
        "model": body.model.unwrap_or_default(),
        "workspace_path": "",
        "workspace_mode": workspace_mode,
        "created_at": now.clone(),
        "updated_at": now,
        "success": true,
    })))
}

pub(super) fn enriched_messages(
    conn: &rusqlite::Connection,
    id: &str,
) -> anyhow::Result<Vec<serde_json::Value>> {
    let messages = crate::db::message_repo::get_messages_by_conversation(conn, id)?;
    messages
        .iter()
        .map(|msg| {
            let mut value = serde_json::to_value(msg)?;
            value["content"] = crate::db::message_repo::user_content_for_display(conn, msg)?;
            value["requestSnapshots"] =
                crate::db::message_repo::get_request_snapshots(conn, &msg.id)
                    .unwrap_or_else(|_| serde_json::json!([]));
            if let Some(turn) = deepseek_code_core::db::turn_repo::get_message_turn(conn, &msg.id)?
            {
                value["turn"] = serde_json::to_value(turn)?;
            }
            let tool_calls = crate::db::message_repo::get_tool_calls_for_message(conn, &msg.id)?;
            if !tool_calls.is_empty() {
                value["tool_calls"] = serde_json::json!(tool_calls
                    .iter()
                    .map(|tc| {
                        let input = tc
                            .input
                            .as_ref()
                            .and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
                            .unwrap_or(serde_json::json!({}));
                        let mcp_result = crate::services::subagents::typed_mcp_result(
                            conn, id, &tc.id, tc.output.as_deref(), tc.is_error,
                        )?;
                        Ok::<_, anyhow::Error>(serde_json::json!({
                            "id": tc.id.strip_prefix(&format!("{}:", msg.id)).unwrap_or(&tc.id),
                            "name": tc.name,
                            "input": input,
                            "result": tc.output,
                            "mcp_result": mcp_result,
                            "status": if tc.is_error { "error" } else if tc.output.is_none() { "running" } else { "done" },
                        }))
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?);
            }
            Ok(value)
        })
        .collect()
}

/// Request snapshot fields that repeat across model requests: the system prompt
/// and tool schemas are usually identical for every request in a session.
const SHARED_SNAPSHOT_FIELDS: [&str; 3] = ["systemPrompt", "systemMessages", "tools"];
/// Smaller values are cheaper inline than as a reference.
const SHARED_PART_MIN_BYTES: usize = 256;

/// Moves repeated large snapshot fields into one table, so a long session sends
/// each distinct system prompt and tool list once instead of once per request.
/// A moved field is removed and `<field>Ref` names its entry in the returned map.
pub(super) fn share_snapshot_parts(messages: &mut [serde_json::Value]) -> serde_json::Map<String, serde_json::Value> {
    let mut parts = serde_json::Map::new();
    let mut ids = std::collections::HashMap::<String, String>::new();
    for message in messages.iter_mut() {
        let Some(snapshots) = message.get_mut("requestSnapshots").and_then(|v| v.as_array_mut()) else { continue };
        for snapshot in snapshots.iter_mut().filter_map(|v| v.as_object_mut()) {
            for field in SHARED_SNAPSHOT_FIELDS {
                let Some(value) = snapshot.get(field) else { continue };
                let key = value.to_string();
                if key.len() < SHARED_PART_MIN_BYTES {
                    continue;
                }
                let next = format!("p{}", ids.len());
                let id = ids.entry(key).or_insert(next).clone();
                if let Some(value) = snapshot.remove(field) {
                    parts.entry(id.clone()).or_insert(value);
                }
                snapshot.insert(format!("{field}Ref"), serde_json::Value::String(id));
            }
        }
    }
    parts
}

#[derive(Deserialize, Default)]
struct ConversationQuery {
    /// Set by clients that expand `snapshotParts` (see `share_snapshot_parts`).
    #[serde(default)]
    snapshot_parts: Option<String>,
}

async fn conversation_get(Path(id): Path<String>, Query(query): Query<ConversationQuery>, State(state): State<AppState>) -> Json<serde_json::Value> {
    let share_parts = query.snapshot_parts.as_deref() == Some("1");
    let db = state.db_manager.clone();
    let id_clone = id.clone();
    let result = tokio::task::spawn_blocking(move || {
        db.with_conn(|conn| {
            let conv = crate::db::conversation_repo::get_conversation(conn, &id_clone).ok().flatten();
            let enriched_messages = enriched_messages(conn, &id_clone)?;

            Ok::<_, anyhow::Error>((conv, enriched_messages))
        })
    }).await;
    let result = result.map(|inner| inner.map(|inner| inner.map(|(conv, mut messages)| {
        let parts = share_parts.then(|| share_snapshot_parts(&mut messages));
        (conv, messages, parts)
    })));
    let with_parts = |mut value: serde_json::Value, parts: Option<serde_json::Map<String, serde_json::Value>>| {
        if let Some(parts) = parts {
            value["snapshotParts"] = serde_json::Value::Object(parts);
        }
        Json(value)
    };
    match result {
        Ok(Ok(Ok((Some(conv), messages, parts)))) => with_parts(serde_json::json!({
            "id": conv.id,
            "title": conv.title,
            "model": conv.model,
            "workspace_path": conv.workspace_path,
            "project_id": conv.project_id,
            "workspace_mode": conv.workspace_mode,
            "research_mode": conv.research_mode,
            "pinned": conv.pinned,
            "archived": conv.archived,
            "created_at": conv.created_at,
            "updated_at": conv.updated_at,
            "message_count": conv.message_count,
            "messages": messages,
        }), parts),
        Ok(Ok(Ok((None, messages, parts)))) => with_parts(serde_json::json!({ "id": id, "messages": messages }), parts),
        _ => Json(serde_json::json!({ "id": id, "messages": [] })),
    }
}

async fn conversation_update(Path(id): Path<String>, State(state): State<AppState>, Json(body): Json<serde_json::Value>) -> Json<serde_json::Value> {
    let db = state.db_manager.clone();
    let id_for_error = id.clone();
    let id_clone = id.clone();
    let result = tokio::task::spawn_blocking(move || {
        db.with_conn(|conn: &rusqlite::Connection| {
            let transaction = deepseek_code_core::db::write_transaction(&conn)?;
            let conn = &transaction;
            anyhow::ensure!(crate::db::conversation_repo::get_conversation(conn, &id_clone)?.is_some(), "Conversation not found");
            if let Some(mode) = body.get("workspace_mode") {
                let mode: crate::db::conversation_repo::WorkspaceMode = serde_json::from_value(mode.clone())?;
                crate::db::conversation_repo::set_workspace_mode(conn, &id_clone, mode.as_str())?;
                crate::db::conversation_repo::update_conversation_timestamp(conn, &id_clone, &chrono::Utc::now().to_rfc3339())?;
            }
            if let Some(pinned) = body.get("pinned").and_then(|v| v.as_bool()) {
                crate::db::conversation_repo::pin_conversation(conn, &id_clone, pinned)?;
            }
            if let Some(archived) = body.get("archived").and_then(|v| v.as_bool()) {
                crate::db::conversation_repo::archive_conversation(conn, &id_clone, archived)?;
            }
            if let Some(project_id) = body.get("project_id") {
                if let Some(project_id) = project_id.as_str() {
                    let project = crate::db::project_repo::require_trusted(conn, project_id)?;
                    conn.execute("DELETE FROM conversation_worktrees WHERE conversation_id = ?1 AND project_id != ?2", rusqlite::params![&id_clone, project_id])?;
                    conn.execute("UPDATE conversations SET project_id = ?1, workspace_path = COALESCE((SELECT path FROM conversation_worktrees WHERE conversation_id = ?4), ?2), updated_at = ?3 WHERE id = ?4",
                        rusqlite::params![project_id, project.workspace_path, chrono::Utc::now().to_rfc3339(), &id_clone])?;
                } else if project_id.is_null() {
                    conn.execute("DELETE FROM conversation_worktrees WHERE conversation_id = ?1", rusqlite::params![&id_clone])?;
                    conn.execute("UPDATE conversations SET project_id = NULL, workspace_path = NULL WHERE id = ?1", rusqlite::params![&id_clone])?;
                }
            }
            if let Some(title) = body.get("title").and_then(|v| v.as_str()) {
                conn.execute(
                    "UPDATE conversations SET title = ?1, updated_at = ?2 WHERE id = ?3",
                    rusqlite::params![title, chrono::Utc::now().to_rfc3339(), &id_clone],
                )?;
            }
            if let Some(model) = body.get("model").and_then(|v| v.as_str()) {
                conn.execute(
                    "UPDATE conversations SET model = ?1, updated_at = ?2 WHERE id = ?3",
                    rusqlite::params![model, chrono::Utc::now().to_rfc3339(), &id_clone],
                )?;
            }
            if let Some(workspace) = body.get("workspace_path").and_then(|v| v.as_str()) {
                conn.execute(
                    "UPDATE conversations SET workspace_path = ?1, updated_at = ?2 WHERE id = ?3",
                    rusqlite::params![workspace, chrono::Utc::now().to_rfc3339(), &id_clone],
                )?;
            }
            if let Some(messages) = body.get("messages").and_then(|v| v.as_array()) {
                let tx = conn;
                let existing = crate::db::message_repo::get_messages_by_conversation(tx, &id_clone)?;
                let mut retained = std::collections::HashSet::new();
                for (idx, msg) in messages.iter().enumerate() {
                    let msg_id = msg.get("id").and_then(|v| v.as_str()).map(str::to_owned)
                        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                    anyhow::ensure!(retained.insert(msg_id.clone()), "Duplicate message ID");
                    let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("user");
                    let previous = crate::db::message_repo::get_message(tx, &msg_id)?;
                    if let Some(previous) = &previous {
                        anyhow::ensure!(previous.conversation_id == id_clone && previous.role == role,
                            "Message ID belongs to a different conversation or role");
                    }
                    let now = chrono::Utc::now().to_rfc3339();
                    let content = msg.get("content").cloned().unwrap_or(serde_json::json!(""));
                    if role == "user" {
                        crate::db::message_repo::save_user_message(tx, &msg_id, &id_clone, &content, &now, idx as i64)?;
                    } else {
                        let content = content.as_str().map(str::to_owned).unwrap_or_else(|| content.to_string());
                        if previous.is_some() {
                            tx.execute("UPDATE messages SET content = ?1, sort_order = ?2 WHERE id = ?3",
                                rusqlite::params![content, idx as i64, msg_id])?;
                        } else {
                            crate::db::message_repo::insert_message(tx, &msg_id, &id_clone, role, &content, None, &now, false, idx as i64)?;
                        }
                    }
                }
                for message in existing {
                    if !retained.contains(&message.id) {
                        crate::db::message_repo::delete_message(tx, &message.id)?;
                    }
                }
                tx.execute("UPDATE conversations SET message_count = ?1, updated_at = ?2 WHERE id = ?3",
                    rusqlite::params![messages.len() as i64, chrono::Utc::now().to_rfc3339(), id_clone])?;
            }
            transaction.commit()?;
            Ok::<(), anyhow::Error>(())
        })
    }).await;
    match result {
        Ok(Ok(Ok(()))) => Json(serde_json::json!({ "ok": true })),
        Err(e) => {
            tracing::error!(module = "Conversations", "Failed to update conversation {}: {}", id_for_error, e);
            Json(serde_json::json!({ "ok": false, "error": format!("{}", e) }))
        }
        Ok(Ok(Err(e))) => {
            tracing::error!(module = "Conversations", "Failed to update conversation {}: {}", id_for_error, e);
            Json(serde_json::json!({ "ok": false, "error": format!("{}", e) }))
        }
        Ok(Err(e)) => {
            tracing::error!(module = "Conversations", "DB error on update {}: {}", id_for_error, e);
            Json(serde_json::json!({ "ok": false, "error": format!("{}", e) }))
        }
    }
}

async fn conversation_delete(Path(id): Path<String>, State(state): State<AppState>) -> Json<serde_json::Value> {
    let db = state.db_manager.clone();
    let id_for_error = id.clone();
    let result = tokio::task::spawn_blocking(move || {
        db.with_conn(|conn| {
            crate::db::message_repo::delete_messages_from(conn, &id, 0).ok();
            crate::db::conversation_repo::delete_conversation(conn, &id)
        })
    }).await;
    match result {
        Ok(Ok(Ok(()))) => Json(serde_json::json!({ "ok": true })),
        Err(e) => {
            tracing::error!(module = "Conversations", "Failed to delete conversation {}: {}", id_for_error, e);
            Json(serde_json::json!({ "ok": false, "error": format!("{}", e) }))
        }
        Ok(Ok(Err(e))) => {
            tracing::error!(module = "Conversations", "Failed to delete conversation {}: {}", id_for_error, e);
            Json(serde_json::json!({ "ok": false, "error": format!("{}", e) }))
        }
        Ok(Err(e)) => {
            tracing::error!(module = "Conversations", "DB error on delete {}: {}", id_for_error, e);
            Json(serde_json::json!({ "ok": false, "error": format!("{}", e) }))
        }
    }
}

async fn conversation_messages(Path(id): Path<String>, State(state): State<AppState>) -> Json<serde_json::Value> {
    let db = state.db_manager.clone();
    let result = tokio::task::spawn_blocking(move || {
        db.with_conn(|conn| enriched_messages(conn, &id))
    }).await;
    match result {
        Ok(Ok(Ok(messages))) => Json(serde_json::json!({ "messages": messages })),
        _ => Json(serde_json::json!({ "messages": [] })),
    }
}

async fn conversation_message_delete(
    Path((id, mid)): Path<(String, String)>,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let db = state.db_manager.clone();
    let result = tokio::task::spawn_blocking(move || {
        db.with_conn(|conn| {
            let msg = crate::db::message_repo::get_message(conn, &mid)?;
            if let Some(m) = msg {
                crate::db::message_repo::delete_messages_from(conn, &id, m.sort_order)?;
            }
            enriched_messages(conn, &id)
        })
    }).await;
    match result {
        Ok(Ok(Ok(messages))) => Ok(Json(serde_json::json!({ "success": true, "messages": messages }))),
        Ok(Ok(Err(e))) => { tracing::error!(module = "MessageDelete", "Failed: {}", e); Err(StatusCode::INTERNAL_SERVER_ERROR) }
        Ok(Err(e)) => { tracing::error!(module = "MessageDelete", "DB lock error: {}", e); Err(StatusCode::INTERNAL_SERVER_ERROR) }
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

async fn conversation_messages_tail_delete(
    Path((id, count)): Path<(String, i64)>,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let db = state.db_manager.clone();
    let result = tokio::task::spawn_blocking(move || {
        db.with_conn(|conn| {
            crate::db::message_repo::delete_messages_tail(conn, &id, count)?;
            enriched_messages(conn, &id)
        })
    }).await;
    match result {
        Ok(Ok(Ok(messages))) => Ok(Json(serde_json::json!({ "success": true, "messages": messages }))),
        Ok(Ok(Err(e))) => { tracing::error!(module = "MessagesTailDelete", "Failed: {}", e); Err(StatusCode::INTERNAL_SERVER_ERROR) }
        Ok(Err(e)) => { tracing::error!(module = "MessagesTailDelete", "DB lock error: {}", e); Err(StatusCode::INTERNAL_SERVER_ERROR) }
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

#[derive(Deserialize)]
struct BranchRequest {
    from_message_id: Option<String>,
    from_message_index: Option<usize>,
}

async fn conversation_branch_handler(
    Path(id): Path<String>,
    State(state): State<AppState>,
    Json(req): Json<BranchRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let db = state.db_manager.clone();
    let result = tokio::task::spawn_blocking(move || {
        db.with_conn(|conn| {
            crate::db::conversation_repo::branch_conversation(conn, &id, req.from_message_id.as_deref(), req.from_message_index)
        })
    }).await;
    match result {
        Ok(Ok(Ok(new_id))) => Ok(Json(serde_json::json!({ "success": true, "new_conversation_id": new_id }))),
        Ok(Ok(Err(e))) => { tracing::error!(module = "Branch", "Failed: {}", e); Err(StatusCode::INTERNAL_SERVER_ERROR) }
        Ok(Err(e)) => { tracing::error!(module = "Branch", "DB lock error: {}", e); Err(StatusCode::INTERNAL_SERVER_ERROR) }
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

#[derive(Deserialize)]
struct AnswerRequest {
    request_id: String,
    tool_use_id: Option<String>,
    answers: Option<serde_json::Value>,
}

async fn conversation_answer_handler(
    Path(id): Path<String>,
    State(state): State<AppState>,
    Json(req): Json<AnswerRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let engine_pool = state.engine_pool.clone();
    let mut pool: tokio::sync::MutexGuard<'_, EnginePool> = engine_pool.lock().await;

    if let Some(engine) = state.native_engine.lock().await.as_ref() {
        drop(pool);
        let tool = req.tool_use_id.as_deref().ok_or(StatusCode::BAD_REQUEST)?;
        let answer = req.answers.as_ref().ok_or(StatusCode::BAD_REQUEST)?.to_string();
        return engine.resume_scoped_answer(&id, &req.request_id, tool, answer, false).await
            .map(Json).map_err(|error| {
                tracing::warn!("Question response rejected: {error:#}");
                StatusCode::CONFLICT
            });
    }
    let original_input = pool.get_ask_user_pending(&id).unwrap_or(serde_json::json!({}));

    let answers = req.answers.unwrap_or(serde_json::json!({}));

    let mut updated_input = original_input;
    if let Some(obj) = updated_input.as_object_mut() {
        obj.insert("answers".to_string(), answers.clone());
    } else {
        updated_input = serde_json::json!({ "answers": answers.clone() });
    }

    let tool_use_id = req.tool_use_id.unwrap_or_default();

    match pool.send_control_response(&id, &req.request_id, &tool_use_id, updated_input).await {
        Ok(()) => Ok(Json(serde_json::json!({ "ok": true }))),
        Err(_) => {
            drop(pool);
            let native_engine = state.native_engine.clone();
            let engine_guard: tokio::sync::MutexGuard<'_, Option<NativeEngine>> = native_engine.lock().await;
            if let Some(engine) = engine_guard.as_ref() {
                let answer_str = serde_json::to_string(&answers).unwrap_or_default();
                match engine.resume_scoped_answer(&id, &req.request_id, &tool_use_id, answer_str, false).await.map(|_| ()) {
                    Ok(()) => Ok(Json(serde_json::json!({ "ok": true }))),
                    Err(e) => {
                        tracing::error!(module = "AskUser", "Native engine answer failed: {}", e);
                        Err(StatusCode::NOT_FOUND)
                    }
                }
            } else {
                tracing::error!(module = "AskUser", "No engine available for conversation {}", id);
                Err(StatusCode::NOT_FOUND)
            }
        }
    }
}

async fn conversation_pending_questions(Path(id): Path<String>, State(state): State<AppState>) -> Json<serde_json::Value> {
    let pending = match state.native_engine.lock().await.as_ref() {
        Some(engine) => engine.pending_questions(&id).await,
        None => Vec::new(),
    };
    Json(serde_json::json!({"requests":pending}))
}

async fn conversation_warm_handler(
    Path(_id): Path<String>,
    State(state): State<AppState>,
    Json(req): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // Current clients never send permissions on navigation. Legacy warm clients
    // may initialize an idle engine, but cannot change any running conversation.
    if let Some(mode) = req["permission_mode"].as_str() {
        let mode = PermissionMode::parse(mode).map_err(|_| StatusCode::BAD_REQUEST)?;
        if mode == PermissionMode::Auto { return Err(StatusCode::NOT_IMPLEMENTED); }
        if mode == PermissionMode::BypassPermissions && !state.permission_manager.bypass_available() {
            return Err(StatusCode::FORBIDDEN);
        }
        if let Some(engine) = state.native_engine.lock().await.as_ref() {
            engine.warm_permission_mode(mode).await;
        }
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
struct PermissionRequest {
    request_id: String,
    tool_use_id: Option<String>,
    behavior: Option<String>,
}

async fn conversation_bash_session(Path(id): Path<String>, State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({"allowed": state.permission_manager.bash_session_allowed(&id)}))
}

async fn conversation_bash_session_revoke(Path(id): Path<String>, State(state): State<AppState>) -> Json<serde_json::Value> {
    state.permission_manager.set_bash_session_allowed(&id, false);
    Json(serde_json::json!({"allowed": false}))
}

async fn conversation_permission_handler(
    Path(id): Path<String>,
    State(state): State<AppState>,
    Json(req): Json<PermissionRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let engine_pool = state.engine_pool.clone();
    let mut pool: tokio::sync::MutexGuard<'_, EnginePool> = engine_pool.lock().await;

    if let Some(engine) = state.native_engine.lock().await.as_ref() {
        drop(pool);
        let tool = req.tool_use_id.as_deref().ok_or(StatusCode::BAD_REQUEST)?;
        let behavior = req.behavior.as_deref().ok_or(StatusCode::BAD_REQUEST)?;
        if !matches!(behavior, "allow" | "deny" | "allow_session") { return Err(StatusCode::BAD_REQUEST); }
        return engine.resume_scoped_answer(&id, &req.request_id, tool, behavior.into(), true).await
            .map(Json).map_err(|_| StatusCode::CONFLICT);
    }
    let pending = pool.get_tool_permission_pending(&id);
    let tool_use_id = req.tool_use_id
        .or_else(|| pending.as_ref().and_then(|p| p.get("tool_use_id").and_then(|t| t.as_str()).map(String::from)))
        .unwrap_or_default();

    let behavior = req.behavior.unwrap_or_else(|| "allow".to_string());
    if behavior == "allow_session" { return Err(StatusCode::NOT_IMPLEMENTED); }

    let updated_input = pending.and_then(|p| p.get("input").cloned());

    // Try EnginePool first (legacy external process mode)
    let pool_result = pool.send_permission_response(&id, &req.request_id, &tool_use_id, &behavior, updated_input).await;
    if pool_result.is_ok() {
        pool.remove_tool_permission_pending(&id);
        return Ok(Json(serde_json::json!({ "ok": true })));
    }

    // EnginePool failed, try NativeEngine (new Rust-native mode)
    drop(pool);
    let native_engine = state.native_engine.clone();
    let engine_guard: tokio::sync::MutexGuard<'_, Option<NativeEngine>> = native_engine.lock().await;
    if let Some(engine) = engine_guard.as_ref() {
        let answer = if behavior == "allow" { "allow".to_string() } else { "deny".to_string() };
        match engine.resume_scoped_answer(&id, &req.request_id, &tool_use_id, answer, true).await.map(|_| ()) {
            Ok(()) => Ok(Json(serde_json::json!({ "ok": true }))),
            Err(e) => {
                tracing::error!(module = "Permission", "Native engine answer failed: {}", e);
                Err(StatusCode::NOT_FOUND)
            }
        }
    } else {
        tracing::error!(module = "Permission", "No pool engine and no native engine for conversation {}", id);
        Err(StatusCode::NOT_FOUND)
    }
}

/// Estimated context occupancy of a persisted conversation and its model window.
async fn conversation_context_size(
    Path(id): Path<String>,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let db = state.db_manager.clone();
    let id_clone = id.clone();
    let measured = tokio::task::spawn_blocking(move || {
        let conversation = db.with_conn(|conn| crate::db::conversation_repo::get_conversation(conn, &id_clone))??;
        let store = crate::runtime::SessionStore::new(db);
        let tokens = crate::services::context_usage::estimated_tokens(&store, &id_clone)?;
        Ok::<_, anyhow::Error>(conversation.zip(tokens))
    }).await;
    let (conversation, tokens) = match measured {
        Ok(Ok(Some(found))) => found,
        Ok(Ok(None)) => return Err((StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "Conversation not found" })))),
        Ok(Err(e)) => {
            tracing::error!(module = "ContextSize", "Failed for {}: {}", id, e);
            return Err((StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() }))));
        }
        Err(e) => return Err((StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({ "error": e.to_string() })))),
    };
    // Resolve against the configured providers, as each chat request does.
    let providers = match state.config_manager.lock().await.as_ref() {
        Some(cm) => cm.get_config().providers.iter().map(crate::runtime::provider_from_config).collect(),
        None => Vec::new(),
    };
    let providers = crate::native_engine::provider_manager::ProviderManager::in_memory(providers, std::path::PathBuf::new());
    let model = conversation.model.unwrap_or_default();
    let limit = crate::services::context_usage::context_limit(&providers, conversation.provider.as_deref(), &model).await;
    Ok(Json(serde_json::json!({ "tokens": tokens, "limit": limit })))
}

async fn conversation_generation_status(Path(id): Path<String>, State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(state.generation_runs.generation_status(&id))
}

async fn conversation_stream_status(Path(id): Path<String>, State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(state.generation_runs.stream_status(&id))
}

/// Stop preserves pending input and pauses backend dispatch until explicit resume.
async fn conversation_stop_generation(Path(id): Path<String>, State(state): State<AppState>) -> axum::response::Response {
    super::input_queue::stop(state, id).await
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/conversations", get(conversations_list))
        .route("/api/conversations", post(conversations_create))
        .route("/api/conversations/{id}", get(conversation_get))
        .route("/api/conversations/{id}", post(conversation_update))
        .route("/api/conversations/{id}", delete(conversation_delete))
        .route("/api/conversations/{id}/messages", get(conversation_messages))
        .route("/api/conversations/{id}/messages/{mid}", delete(conversation_message_delete))
        .route("/api/conversations/{id}/messages-tail/{count}", delete(conversation_messages_tail_delete))
        .route("/api/conversations/{id}/branch", post(conversation_branch_handler))
        .route("/api/conversations/{id}/answer", post(conversation_answer_handler))
        .route("/api/conversations/{id}/pending-questions", get(conversation_pending_questions))
        .route("/api/conversations/{id}/permission", post(conversation_permission_handler))
        .route("/api/conversations/{id}/bash-session", get(conversation_bash_session).delete(conversation_bash_session_revoke))
        .route("/api/conversations/{id}/warm", post(conversation_warm_handler))
        .route("/api/conversations/{id}/context-size", get(conversation_context_size))
        .route("/api/conversations/{id}/generation-status", get(conversation_generation_status))
        .route("/api/conversations/{id}/stream-status", get(conversation_stream_status))
        .route("/api/conversations/{id}/stop-generation", post(conversation_stop_generation))
}

#[cfg(test)]
mod snapshot_part_tests {
    use super::share_snapshot_parts;
    use serde_json::json;

    #[test]
    fn repeated_prompts_and_tools_are_sent_once() {
        let prompt = "system ".repeat(100);
        let tools = json!([{"name": "Bash", "description": "run ".repeat(100), "input_schema": {}}]);
        let snapshot = |id: &str, prompt: &str| json!({"id": id, "systemPrompt": prompt, "tools": tools, "systemMessages": [], "model": "m"});
        let mut messages = vec![
            json!({"role": "user"}),
            json!({"role": "assistant", "requestSnapshots": [snapshot("a", &prompt), snapshot("b", &prompt)]}),
            json!({"role": "assistant", "requestSnapshots": [snapshot("c", &format!("{prompt} changed"))]}),
        ];
        let parts = share_snapshot_parts(&mut messages);
        // One tool list and two distinct prompts.
        assert_eq!(parts.len(), 3);
        let a = &messages[1]["requestSnapshots"][0];
        let b = &messages[1]["requestSnapshots"][1];
        let c = &messages[2]["requestSnapshots"][0];
        assert!(a.get("systemPrompt").is_none() && a.get("tools").is_none());
        assert_eq!(a["systemPromptRef"], b["systemPromptRef"]);
        assert_ne!(a["systemPromptRef"], c["systemPromptRef"]);
        assert_eq!(a["toolsRef"], c["toolsRef"]);
        assert_eq!(parts[a["systemPromptRef"].as_str().unwrap()], json!(prompt));
        assert_eq!(parts[c["toolsRef"].as_str().unwrap()], tools);
        // Small values stay inline.
        assert_eq!(a["systemMessages"], json!([]));
        assert_eq!(a["model"], "m");
    }
}
