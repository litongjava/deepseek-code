use super::AppState;
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use deepseek_code_core::{
    db::stored_binding::StoredBinding,
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

pub(super) enum ModelRequest<'a> {
    Chat {
        model: &'a str,
        provider: Option<&'a str>,
    },
    Compact(&'a deepseek_code_core::db::session_store::SessionDb),
}

/// The Desktop sends either a decoded provider/model pair or a persisted
/// encodeURIComponent(provider)::model key. Keep the requested model selector
/// (including virtual context-window suffixes) through the second resolution.
pub(super) async fn resolve_model(
    state: &AppState,
    engine: &NativeEngine,
    id: &str,
    request: ModelRequest<'_>,
) -> Result<ChatModel, Response> {
    let (stored, pending) = state
        .db_manager
        .stored_binding(id)
        .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e))?;
    if pending {
        return Err(error(
            StatusCode::CONFLICT,
            "Conversation provider switch is pending",
        ));
    }
    let compact = matches!(request, ModelRequest::Compact(_));
    let (mut selection, recorded) = match request {
        ModelRequest::Compact(session) => {
            let recorded = if stored.provider.is_none() {
                session
                    .read(|r| r.last_completed_turn_provider())
                    .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e))?
            } else {
                None
            };
            (stored, recorded)
        }
        ModelRequest::Chat {
            model,
            provider: Some(id),
        } => (
            StoredBinding::from_row(
                Some(id.to_owned()),
                if model.is_empty() {
                    stored.model
                } else {
                    model.to_owned()
                },
            ),
            None,
        ),
        ModelRequest::Chat { model, .. } if !model.is_empty() && model != stored.model => (
            StoredBinding::desktop_selection(model)
                .map_err(|e| error(StatusCode::BAD_REQUEST, e))?,
            None,
        ),
        ModelRequest::Chat { model, .. } => {
            let recorded = if stored.provider.is_none() && stored.model.contains("::") {
                state
                    .db_manager
                    .open_session(id)
                    .and_then(|s| s.read(|r| r.last_completed_turn_provider()))
                    .map_err(|e| error(StatusCode::INTERNAL_SERVER_ERROR, e))?
            } else {
                None
            };
            if recorded.is_none() && stored.provider.is_none() && !model.is_empty() {
                // The caller explicitly supplied a selector for an unbound chat.
                (
                    StoredBinding::desktop_selection(model)
                        .map_err(|e| error(StatusCode::BAD_REQUEST, e))?,
                    None,
                )
            } else {
                (stored, recorded)
            }
        }
    };
    if !compact && selection.model.is_empty() {
        selection.model = "claude-sonnet-4-6".into();
    }
    let pinned = compact || selection.provider.is_some() || recorded.is_some();
    let (resolved, selector) = if pinned {
        selection
            .resolve_pinned(recorded, engine)
            .await
            .map_err(|e| {
                if compact {
                    error(StatusCode::CONFLICT, e)
                } else {
                    error(StatusCode::BAD_REQUEST, "Chat model unavailable")
                }
            })?
    } else {
        // Automatic routing is a chat-only option for an unqualified model.
        // A legacy stored prefix alone supplies no provider authority.
        if selection.model.contains("::") {
            return Err(error(
                StatusCode::BAD_REQUEST,
                "Pick a model in Desktop before chatting",
            ));
        }
        let resolved = engine
            .resolve_provider(&selection.model)
            .await
            .ok_or_else(|| error(StatusCode::BAD_REQUEST, "Chat model unavailable"))?;
        (resolved, selection.model)
    };
    Ok(ChatModel {
        provider_id: resolved.provider.id,
        api_format: resolved.provider.api_format,
        selector,
        explicit_provider: pinned,
    })
}
