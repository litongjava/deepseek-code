use super::super::chat_admission::{self, error, Operation};
use super::AppState;
use anyhow::Context;
use axum::{
    body::Bytes,
    extract::{FromRequest, Path, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
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
    #[serde(default, rename = "env_token")]
    _env_token: Option<serde::de::IgnoredAny>,
    #[serde(default, rename = "env_base_url")]
    _env_base_url: Option<serde::de::IgnoredAny>,
    #[serde(flatten)]
    unknown: std::collections::BTreeMap<String, serde::de::IgnoredAny>,
}

pub(super) async fn handler(
    Path(id): Path<String>,
    State(state): State<AppState>,
    request: Request,
) -> Result<Json<Value>, Response> {
    // An omitted body means no focus. Nonempty bytes require JSON even when
    // the caller forgot Content-Type; never silently discard a supplied focus.
    let body = if request
        .headers()
        .contains_key(axum::http::header::CONTENT_TYPE)
    {
        Json::<Body>::from_request(request, &state)
            .await
            .map_err(IntoResponse::into_response)?
            .0
    } else {
        let bytes = Bytes::from_request(request, &state)
            .await
            .map_err(IntoResponse::into_response)?;
        if !bytes.is_empty() {
            return Err(error(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "Expected application/json",
            ));
        }
        Body::default()
    };
    if let Some(field) = body.unknown.keys().next() {
        return Err(error(
            StatusCode::BAD_REQUEST,
            format!("Unknown compact field: {field}"),
        ));
    }
    let db = &state.db_manager;
    let internal = |e: anyhow::Error| error(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"));
    let gate = super::super::input_queue::gate(&state, &id).await;
    if state.generation_runs.is_active(&id) {
        return Err(error(StatusCode::CONFLICT, "Conversation is running"));
    }
    let (_lease, _) = chat_admission::admit(&state, &id, Operation::Compact).await?;
    drop(gate);
    let session = db.open_session(&id).map_err(internal)?;
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
    let workspace = session.workspace().map_err(internal)?;
    let engine = NativeEngine::new(
        Arc::new(tokio::sync::Mutex::new(manager)),
        db.clone(),
        paths.layout().workspaces(),
        state.permission_manager.clone(),
        Arc::new(FileCache::new()),
    );
    let selected = chat_admission::resolve_model(
        &state,
        &engine,
        &id,
        chat_admission::ModelRequest::Compact(&session),
    )
    .await?;
    let context = ServiceContext {
        paths,
        db: db.clone(),
        credentials: Arc::new(EnvironmentCredentials),
    };
    let store = SessionStore::new(db.clone());
    let tokens = || {
        context_usage::estimated_tokens(
            &store,
            &id,
            &selected.selector,
            Some(&selected.provider_id),
        )
    };
    let before = tokens().map_err(internal)?.unwrap_or_default();
    let instruction = body.instruction.unwrap_or_default();
    let report = compact_with_hooks(
        &engine,
        &context,
        &workspace,
        &id,
        &selected.provider_id,
        &selected.selector,
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
