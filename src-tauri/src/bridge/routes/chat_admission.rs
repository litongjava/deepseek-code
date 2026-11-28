use super::AppState;
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use deepseek_code_core::{
    native_engine::{provider_manager::ApiFormat, NativeEngine},
    services::session_messages::ExecutionLease,
};
use serde_json::{json, Value};

pub(super) enum Operation<'a> {
    Chat {
        model: &'a str,
        provider: Option<&'a str>,
        content: Option<&'a Value>,
        queued: bool,
    },
    Compact,
}

pub(super) fn error(status: StatusCode, reason: impl std::fmt::Display) -> Response {
    (status, Json(json!({"error": reason.to_string()}))).into_response()
}

/// Admission under the caller's per-conversation gate. Only chat can create a
/// conversation or consume a sealed first message.
pub(super) async fn admit(
    state: &AppState,
    id: &str,
    operation: Operation<'_>,
) -> Result<(ExecutionLease, bool), Response> {
    if !state.rate_limiter.check("chat_handler").await {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            [("X-RateLimit-Remaining", "0"), ("Retry-After", "60")],
            Json(json!({"error":"Rate limit exceeded"})),
        )
            .into_response());
    }
    let db = state.db_manager.clone();
    let project_id = id.to_owned();
    tokio::task::spawn_blocking(move || db.validate_chat_project(&project_id))
        .await
        .map_err(|_| {
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Could not validate conversation project",
            )
        })?
        .map_err(|e| error(StatusCode::BAD_REQUEST, e))?;

    let db = &state.db_manager;
    match operation {
        Operation::Chat {
            model,
            provider,
            content,
            queued,
        } => {
            if let Some(typed) = content.filter(|value| !value.is_string()) {
                crate::runtime::normalize_user_content(typed)
                    .map_err(|e| error(StatusCode::BAD_REQUEST, e))?;
            }
            db.admit_chat_execution_with_provider(
                id,
                model,
                provider,
                content.filter(|_| !queued),
                "typed",
            )
        }
        Operation::Compact => {
            if db
                .conversation_metadata(id)
                .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e))?
                .is_none()
            {
                return Err(error(StatusCode::NOT_FOUND, "Conversation not found"));
            }
            db.open_session(id)
                .and_then(|s| s.acquire_settled_execution())
                .map(|lease| (lease, false))
        }
    }
    .map_err(|e| error(StatusCode::CONFLICT, e))
}

pub(super) struct ChatModel {
    pub provider_id: String,
    pub api_format: ApiFormat,
    pub selector: String,
    // A plain, automatically resolved chat still permits simple-turn routing.
    pub explicit_provider: bool,
}

/// The Desktop sends either a decoded provider/model pair or a persisted
/// encodeURIComponent(provider)::model key. Keep the requested model selector
/// (including virtual context-window suffixes) through the second resolution.
pub(super) async fn resolve_model(
    state: &AppState,
    engine: &NativeEngine,
    id: &str,
    requested: &str,
    provider: Option<&str>,
) -> Result<ChatModel, Response> {
    let current = state
        .db_manager
        .observed_binding(id)
        .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    if current.pending.is_some() {
        return Err(error(
            StatusCode::CONFLICT,
            "Conversation provider switch is pending",
        ));
    }
    let (stored_provider, stored_model) = split_model_key(&current.model);
    let (qualified, model) = if requested.is_empty() {
        (stored_provider, stored_model)
    } else if provider.is_some() {
        // The client has already split this key. The model ID itself is opaque.
        (None, requested)
    } else {
        split_model_key(requested)
    };
    let model = if model.is_empty() {
        "claude-sonnet-4-6"
    } else {
        model
    };
    let same_model = model == stored_model;
    let provider = match provider {
        Some(id) => Some(std::borrow::Cow::Borrowed(id)),
        None => qualified
            .or(stored_provider.filter(|_| same_model))
            .map(urlencoding::decode)
            .transpose()
            .map_err(|e| error(StatusCode::BAD_REQUEST, e))?
            .or_else(|| {
                (same_model && !current.provider.is_empty())
                    .then_some(std::borrow::Cow::Borrowed(current.provider.as_str()))
            }),
    };
    let resolved = match provider.as_deref() {
        Some(id) => engine.resolved_provider(id, model).await.ok(),
        None => engine.resolve_provider(model).await,
    }
    .ok_or_else(|| error(StatusCode::BAD_REQUEST, "Chat model unavailable"))?;
    Ok(ChatModel {
        provider_id: resolved.provider.id,
        api_format: resolved.provider.api_format,
        selector: model.to_owned(),
        explicit_provider: provider.is_some(),
    })
}

fn split_model_key(key: &str) -> (Option<&str>, &str) {
    key.split_once("::")
        .map_or((None, key), |(provider, model)| (Some(provider), model))
}
