use super::AppState;
use anyhow::Context;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use deepseek_code_core::{
    cache::FileCache,
    native_engine::{provider_manager::ProviderManager, NativeEngine},
    runtime::{provider_from_config, RuntimePaths, SessionStore},
    services::{
        compaction::compact_with_hooks, context_usage, EnvironmentCredentials, ServiceContext,
    },
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Default, Deserialize)]
pub(super) struct Body {
    #[serde(default, alias = "focus", alias = "instructions")]
    instruction: Option<String>,
}

type Error = (StatusCode, Json<Value>);

fn error(status: StatusCode, reason: impl std::fmt::Display) -> Error {
    (status, Json(json!({"error":reason.to_string()})))
}

pub(super) async fn handler(
    Path(id): Path<String>,
    State(state): State<AppState>,
    body: Option<Json<Body>>,
) -> Result<Json<Value>, Error> {
    let db = &state.db_manager;
    let internal = |e: anyhow::Error| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
    let gate = super::super::input_queue::gate(&state, &id).await;
    if state.generation_runs.is_active(&id) {
        return Err(error(StatusCode::CONFLICT, "Conversation is running"));
    }
    let metadata = db
        .conversation_metadata(&id)
        .map_err(internal)?
        .ok_or_else(|| error(StatusCode::NOT_FOUND, "Conversation not found"))?;
    let session = db.open_session(&id).map_err(internal)?;
    // Includes other processes and the executor tail after the SSE stream ends.
    let _lease = session
        .acquire_execution()
        .map_err(|e| error(StatusCode::CONFLICT, e))?;
    drop(gate);

    let desktop = match db.desktop_context(&id) {
        Ok(req) => Some(req),
        Err(e)
            if matches!(
                e.downcast_ref::<rusqlite::Error>(),
                Some(rusqlite::Error::QueryReturnedNoRows)
            ) =>
        {
            None
        }
        Err(e) => return Err(internal(e)),
    };
    // The model can be changed in the UI after the last chat request.
    let model = metadata
        .model
        .as_deref()
        .filter(|model| !model.is_empty())
        .or_else(|| {
            desktop
                .as_ref()
                .map(|req| req.model.as_str())
                .filter(|model| !model.is_empty())
        })
        .unwrap_or("claude-sonnet-4-6");
    let provider = desktop
        .as_ref()
        .filter(|req| req.model == model)
        .and_then(|req| req.provider_id.as_deref())
        .or(metadata.provider.as_deref());
    let paths = RuntimePaths::new(state.data_dir.as_ref().clone()).map_err(internal)?;
    let providers = state
        .config_manager
        .lock()
        .await
        .as_ref()
        .context("Configuration unavailable")
        .map_err(internal)?
        .get_config()
        .providers
        .iter()
        .map(provider_from_config)
        .collect();
    // Freeze the configured providers for this operation. Holding the Bridge's
    // global engine mutex across provider backoff would block unrelated chats.
    let manager =
        ProviderManager::in_memory(providers, paths.layout().config_file("providers.json"));
    let selected = match provider {
        Some(_) if model.contains("::") => manager.resolve_provider(model).await,
        Some(provider) => manager.resolve_selected(provider, model),
        None => manager.resolve_provider(model).await,
    }
    .ok_or_else(|| {
        error(
            StatusCode::BAD_REQUEST,
            format!("Compaction model unavailable: {model}"),
        )
    })?;
    let workspace = session.workspace().map_err(internal)?;
    let engine = NativeEngine::new(
        Arc::new(tokio::sync::Mutex::new(manager)),
        db.clone(),
        paths.layout().workspaces(),
        state.permission_manager.clone(),
        Arc::new(FileCache::new()),
    );
    let context = ServiceContext {
        paths,
        db: db.clone(),
        credentials: Arc::new(EnvironmentCredentials),
    };
    let store = SessionStore::new(db.clone());
    let tokens = || context_usage::estimated_tokens(&store, &id, model, provider);
    let before = tokens().map_err(internal)?.unwrap_or_default();
    let instruction = body
        .and_then(|Json(body)| body.instruction)
        .unwrap_or_default();
    let report = compact_with_hooks(
        &engine,
        &context,
        &workspace,
        &id,
        &selected.provider.id,
        &selected.model.id,
        instruction.trim(),
        "manual",
    )
    .await
    .map_err(|e| {
        let status = if e.is::<deepseek_code_core::services::compaction::GenerationFailure>() {
            StatusCode::BAD_GATEWAY
        } else {
            StatusCode::BAD_REQUEST
        };
        error(status, format!("{e:#}"))
    })?;
    let summary = session
        .read(|r| r.user_content(&report.message_id))
        .context("Compaction saved, but its summary could not be read")
        .map_err(internal)?;
    let after = tokens()
        .unwrap_or_else(|error| {
            tracing::warn!(%id, %error, "Could not estimate context after manual compaction");
            None
        })
        .unwrap_or(before);
    Ok(Json(json!({
        "summary": summary,
        "tokensSaved": before.saturating_sub(after),
        "messagesCompacted": report.summarized_messages
    })))
}
