use axum::{extract::Query, http::StatusCode, routing::{delete, get, post}, Json, Router};
use serde::Deserialize;
use super::super::AppState;

#[derive(Deserialize)]
struct DirectoriesQuery {
    path: Option<String>,
    #[serde(default)]
    hidden: bool,
}

/// One level of subdirectories on this machine, for the project folder picker
/// in browsers and the mobile app (Desktop uses the native dialog).
async fn directories(Query(query): Query<DirectoriesQuery>) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    tokio::task::spawn_blocking(move || {
        deepseek_code_core::fs::directories::list_directories(query.path.as_deref(), query.hidden)
    })
    .await
    .map_err(|error| (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": error.to_string()}))))?
    .map(|listing| Json(serde_json::json!(listing)))
    .map_err(|error| (StatusCode::BAD_REQUEST, Json(serde_json::json!({"error": format!("{error:#}")}))))
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/filesystem/tree", get(super::super::fs_tree))
        .route("/api/filesystem/directories", get(directories))
        .route("/api/filesystem/read", get(super::super::fs_read))
        .route("/api/filesystem/write", post(super::super::fs_write))
        .route("/api/filesystem/create", post(super::super::fs_create))
        .route("/api/filesystem/delete", delete(super::super::fs_delete))
}
