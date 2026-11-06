//! GitHub connector routes: connection, repository browsing, and copying files
//! into a conversation's workspace.

use super::super::AppState;
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse},
    routing::{get, post},
    Json, Router,
};
use deepseek_code_core::services::github_connector::Selection;
use serde::Deserialize;
use serde_json::{json, Value};

type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;

fn fail(status: StatusCode, error: impl ToString) -> (StatusCode, Json<Value>) {
    (status, Json(json!({ "error": error.to_string() })))
}

fn bad_gateway(error: anyhow::Error) -> (StatusCode, Json<Value>) {
    let message = error.to_string();
    let status = if message.contains("not connected") { StatusCode::UNAUTHORIZED } else { StatusCode::BAD_GATEWAY };
    fail(status, message)
}

/// This Bridge's callback address as the browser sees it.
fn callback_url(headers: &HeaderMap, https: bool) -> Result<String, (StatusCode, Json<Value>)> {
    let host = headers
        .get("host")
        .and_then(|value| value.to_str().ok())
        .filter(|host| !host.is_empty() && !host.contains(['/', '\\', ' ']))
        .ok_or_else(|| fail(StatusCode::BAD_REQUEST, "Missing Host header"))?;
    let forwarded_https = headers.get("x-forwarded-proto").and_then(|v| v.to_str().ok()) == Some("https");
    let scheme = if https || forwarded_https { "https" } else { "http" };
    Ok(format!("{scheme}://{host}/api/github/callback"))
}

#[derive(Deserialize)]
struct TokenBody {
    token: String,
}

#[derive(Deserialize)]
struct Callback {
    code: Option<String>,
    state: Option<String>,
}

#[derive(Deserialize, Default)]
struct BrowseQuery {
    #[serde(default)]
    path: String,
    #[serde(default, rename = "ref")]
    git_ref: String,
    #[serde(default)]
    page: Option<u32>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MaterializeBody {
    conversation_id: String,
    repo_full_name: String,
    #[serde(default, rename = "ref")]
    git_ref: String,
    selections: Vec<Selection>,
}

fn page(title: &str, message: &str) -> Html<String> {
    let escape = |text: &str| text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
    Html(format!(
        "<!doctype html><meta charset=utf-8><title>{}</title><body style=\"font-family:system-ui,sans-serif;display:flex;align-items:center;justify-content:center;height:100vh;margin:0\"><div style=\"text-align:center\"><h2>{}</h2><p>{}</p></div></body>",
        escape(title), escape(title), escape(message)
    ))
}

pub fn routes(https: bool) -> Router<AppState> {
    Router::new()
        .route("/api/github/status", get(|State(state): State<AppState>| async move { Json(state.github.status()) }))
        .route("/api/github/auth-url", get(move |State(state): State<AppState>, headers: HeaderMap| async move {
            let redirect = callback_url(&headers, https)?;
            state.github.auth_url(&redirect).map(Json).map_err(|e| fail(StatusCode::SERVICE_UNAVAILABLE, e))
        }))
        .route("/api/github/callback", get(|State(state): State<AppState>, Query(query): Query<Callback>| async move {
            let (Some(code), Some(oauth_state)) = (query.code, query.state) else {
                return (StatusCode::BAD_REQUEST, page("GitHub", "Missing code or state.")).into_response();
            };
            match state.github.complete_oauth(&code, &oauth_state).await {
                Ok(token) => page("GitHub connected", &format!("Signed in as {}. You can close this window.", token.login.unwrap_or_default())).into_response(),
                Err(error) => (StatusCode::BAD_REQUEST, page("GitHub connection failed", &error.to_string())).into_response(),
            }
        }))
        .route("/api/github/token", post(|State(state): State<AppState>, Json(body): Json<TokenBody>| async move {
            let token = state.github.connect_with_token(&body.token).await.map_err(|e| fail(StatusCode::BAD_REQUEST, e))?;
            ApiResult::Ok(Json(json!({"connected": true, "user": {"login": token.login, "avatar_url": token.avatar_url, "name": token.name}})))
        }))
        .route("/api/github/disconnect", post(|State(state): State<AppState>| async move {
            state.github.disconnect().map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))?;
            ApiResult::Ok(Json(json!({"ok": true})))
        }))
        .route("/api/github/repos", get(|State(state): State<AppState>, Query(query): Query<BrowseQuery>| async move {
            state.github.repos(query.page.unwrap_or(1)).await.map(Json).map_err(bad_gateway)
        }))
        .route("/api/github/repos/{owner}/{repo}/contents", get(|State(state): State<AppState>, Path((owner, repo)): Path<(String, String)>, Query(query): Query<BrowseQuery>| async move {
            state.github.contents(&owner, &repo, &query.path, &query.git_ref).await.map(Json).map_err(bad_gateway)
        }))
        .route("/api/github/repos/{owner}/{repo}/tree", get(|State(state): State<AppState>, Path((owner, repo)): Path<(String, String)>, Query(query): Query<BrowseQuery>| async move {
            state.github.tree(&owner, &repo, &query.git_ref).await.map(Json).map_err(bad_gateway)
        }))
        .route("/api/github/materialize", post(|State(state): State<AppState>, Json(body): Json<MaterializeBody>| async move {
            let conversation = state
                .db_manager
                .with_conn(|conn| crate::db::conversation_repo::get_conversation(conn, &body.conversation_id))
                .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))?
                .map_err(|e| fail(StatusCode::INTERNAL_SERVER_ERROR, e))?
                .ok_or_else(|| fail(StatusCode::NOT_FOUND, "Conversation not found"))?;
            let workspace = conversation
                .workspace_path
                .filter(|path| !path.is_empty())
                .ok_or_else(|| fail(StatusCode::BAD_REQUEST, "Conversation has no workspace folder"))?;
            state
                .github
                .materialize(std::path::Path::new(&workspace), &body.repo_full_name, &body.git_ref, &body.selections)
                .await
                .map(Json)
                .map_err(bad_gateway)
        }))
}
