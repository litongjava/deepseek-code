//! Durable queue API and backend-owned turn dispatch.
use super::super::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, patch, post},
    Json, Router,
};
use deepseek_code_core::{
    db::DbManager,
    native_engine::engine_core::ChatRequest,
    services::input_queue::{self as queue, QueueError, QueueItem, QueueMode, QueueState},
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
};

// Admission and Stop share a gate; it is not held while a model runs.
pub(super) async fn gate(state: &AppState, id: &str) -> tokio::sync::OwnedMutexGuard<()> {
    static GATES: OnceLock<Mutex<HashMap<(usize, String), Arc<tokio::sync::Mutex<()>>>>> =
        OnceLock::new();
    let key = (Arc::as_ptr(&state.db_manager) as usize, id.to_owned());
    let gate = GATES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(key)
        .or_default()
        .clone();
    gate.lock_owned().await
}

fn error(error: anyhow::Error) -> Response {
    let status = match error.downcast_ref::<QueueError>() {
        Some(QueueError::NotFound) => StatusCode::NOT_FOUND,
        Some(QueueError::Conflict | QueueError::Paused) => StatusCode::CONFLICT,
        Some(_) => StatusCode::BAD_REQUEST,
        None => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (status, Json(json!({"error":error.to_string()}))).into_response()
}
fn exists(db: &DbManager, id: &str) -> anyhow::Result<()> {
    if db
        .with_conn(|conn| crate::db::conversation_repo::get_conversation(conn, id))??
        .is_none()
    {
        return Err(QueueError::NotFound.into());
    }
    Ok(())
}
fn snapshot(db: &DbManager, id: &str) -> anyhow::Result<Value> {
    exists(db, id)?;
    Ok(json!({"items":queue::list(db,id)?,"paused":queue::is_paused(db,id)?}))
}

pub(super) fn execution_lease(
    db: &DbManager,
    id: &str,
) -> anyhow::Result<deepseek_code_core::services::session_messages::ExecutionLease> {
    let path: String =
        db.with_conn(|conn| conn.query_row("PRAGMA database_list", [], |row| row.get(2)))??;
    let path = std::path::Path::new(&path);
    let root = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Queue needs a file-backed database"))?;
    deepseek_code_core::services::session_messages::ExecutionLease::acquire(root, id)?
        .ok_or_else(|| anyhow::anyhow!("Session is executing in another process"))
}

pub(super) fn remember(state: &AppState, req: &ChatRequest) -> anyhow::Result<()> {
    let mut context = req.clone();
    context.messages.clear();
    state.db_manager.with_conn(|conn| -> anyhow::Result<()> {
        conn.execute_batch("CREATE TABLE IF NOT EXISTS desktop_queue_context(session_id TEXT PRIMARY KEY,request_json TEXT NOT NULL)")?;
        conn.execute("INSERT INTO desktop_queue_context VALUES(?1,?2) ON CONFLICT(session_id) DO UPDATE SET request_json=excluded.request_json",
            rusqlite::params![req.conversation_id,serde_json::to_string(&context)?])?;
        Ok(())
    })?
}

pub(super) fn handoff(db: &DbManager, id: &str, item: &QueueItem) -> anyhow::Result<QueueItem> {
    queue::handoff(db, id, item.id, item.revision, |tx, item, message_id| {
        let paused: bool = tx.query_row("SELECT COALESCE((SELECT paused FROM session_input_queue_control WHERE session_id=?1),0)",[id],|r|r.get(0))?;
        anyhow::ensure!(!paused, QueueError::Paused);
        let order: i64 = tx.query_row(
            "SELECT COALESCE(MAX(sort_order),-1)+1 FROM messages WHERE conversation_id=?1",
            [id],
            |r| r.get(0),
        )?;
        crate::db::message_repo::save_user_message(
            tx,
            message_id,
            id,
            &item.content,
            &chrono::Utc::now().to_rfc3339(),
            order,
        )?;
        tx.execute("UPDATE conversations SET message_count=(SELECT COUNT(*) FROM messages WHERE conversation_id=?1) WHERE id=?1",[id])?;
        Ok(())
    })
}

// A plain spawning function breaks the recursive future type between collector and dispatch.
pub(super) fn kick(state: AppState, id: String) {
    tokio::spawn(async move {
        let _gate = gate(&state, &id).await;
        if state.generation_runs.is_active(&id) {
            return;
        }
        if let Err(error) = dispatch(state.clone(), &id).await {
            tracing::error!(session=%id,"Queue dispatch retained durable state: {error:#}");
            let _ = queue::set_paused(&state.db_manager, &id, true);
        }
    });
}
async fn dispatch(state: AppState, id: &str) -> anyhow::Result<()> {
    if queue::is_paused(&state.db_manager, id)? {
        return Ok(());
    }
    let items = queue::list(&state.db_manager, id)?;
    let Some(item) = items
        .into_iter()
        .find(|i| matches!(i.state, QueueState::Queued | QueueState::Claimed))
    else {
        return Ok(());
    };
    let raw: String = state.db_manager.with_conn(|conn| {
        conn.query_row(
            "SELECT request_json FROM desktop_queue_context WHERE session_id=?1",
            [id],
            |r| r.get(0),
        )
    })??;
    let mut req: ChatRequest = serde_json::from_str(&raw)?;
    let format = {
        let config = state.config_manager.lock().await;
        let config = config
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Configuration unavailable"))?;
        let provider = config
            .get_config()
            .providers
            .iter()
            .find(|p| {
                req.provider_id
                    .as_deref()
                    .map(|id| p.id == id)
                    .unwrap_or_else(|| p.models.iter().any(|m| m.id == req.model))
            })
            .ok_or_else(|| anyhow::anyhow!("Queued turn provider unavailable"))?;
        crate::runtime::provider_from_config(provider).api_format
    };
    req.messages =
        crate::runtime::SessionStore::new(state.db_manager.clone()).model_history(id, &format)?;
    req.messages
        .push(json!({"role":"user","content":item.content}));
    // chat owns the same collector/persistence path as an interactive turn. Its body
    // is disposable: collection is detached from any HTTP consumer.
    let response = super::chat::start_chat(state, req, Some(item)).await;
    anyhow::ensure!(
        response.status().is_success(),
        "Queued turn failed admission: {}",
        response.status()
    );
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Admission {
    request_id: String,
    content: Value,
    mode: QueueMode,
}
#[derive(Deserialize)]
struct Mutation {
    revision: i64,
    text: Option<String>,
}
async fn list(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match snapshot(&state.db_manager, &id) {
        Ok(value) => Json(value).into_response(),
        Err(e) => error(e),
    }
}
async fn enqueue(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(input): Json<Admission>,
) -> Response {
    let content = match crate::runtime::normalize_user_content(&input.content) {
        Ok(content) => content,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({"error":e.to_string()})),
            )
                .into_response()
        }
    };
    let result = exists(&state.db_manager, &id).and_then(|_| {
        queue::enqueue(
            &state.db_manager,
            &id,
            &input.request_id,
            content,
            input.mode,
        )
    });
    match result {
        Ok(item) => {
            kick(state, id);
            Json(item).into_response()
        }
        Err(e) => error(e),
    }
}
async fn edit(
    State(state): State<AppState>,
    Path((id, item)): Path<(String, i64)>,
    Json(input): Json<Mutation>,
) -> Response {
    let Some(text) = input.text else {
        return error(QueueError::InvalidContent.into());
    };
    if text.trim().is_empty() {
        return error(QueueError::InvalidContent.into());
    }
    match queue::edit(&state.db_manager, &id, item, input.revision, &text) {
        Ok(item) => Json(item).into_response(),
        Err(e) => error(e),
    }
}
async fn remove(
    State(state): State<AppState>,
    Path((id, item)): Path<(String, i64)>,
    Json(input): Json<Mutation>,
) -> Response {
    match queue::remove(&state.db_manager, &id, item, input.revision) {
        Ok(item) => Json(item).into_response(),
        Err(e) => error(e),
    }
}
async fn steer(
    State(state): State<AppState>,
    Path((id, item)): Path<(String, i64)>,
    Json(input): Json<Mutation>,
) -> Response {
    let _gate = gate(&state, &id).await;
    if !state.generation_runs.is_active(&id) {
        return (
            StatusCode::CONFLICT,
            Json(json!({"error":"Steer requires an active execution; the queued message was retained"})),
        )
            .into_response();
    }
    match queue::mark_steer(&state.db_manager, &id, item, input.revision) {
        Ok(item) => {
            kick(state, id);
            Json(item).into_response()
        }
        Err(e) => error(e),
    }
}
async fn resume(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    let _gate = gate(&state, &id).await;
    match exists(&state.db_manager, &id)
        .and_then(|_| queue::set_paused(&state.db_manager, &id, false))
        .and_then(|_| snapshot(&state.db_manager, &id))
    {
        Ok(value) => {
            kick(state, id);
            Json(value).into_response()
        }
        Err(e) => error(e),
    }
}
pub(super) async fn stop(state: AppState, id: String) -> Response {
    let _gate = gate(&state, &id).await;
    if let Err(e) = queue::set_paused(&state.db_manager, &id, true) {
        return error(e);
    }
    let outcome = state
        .generation_runs
        .stop(&id, std::time::Duration::from_secs(5))
        .await;
    Json(json!({"ok":true,"stopped":outcome.requested>0,"settled":outcome.settled,"paused":true}))
        .into_response()
}
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/conversations/{id}/queue", get(list).post(enqueue))
        .route("/api/conversations/{id}/queue/resume", post(resume))
        .route(
            "/api/conversations/{id}/queue/{item}",
            patch(edit).delete(remove),
        )
        .route("/api/conversations/{id}/queue/{item}/steer", post(steer))
}
