use super::super::AppState;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::post,
    Json, Router,
};
use deepseek_code_core::services::workspace_files::{self as files, FileRef, Scope};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Request {
    scope: Scope,
    root_id: Option<String>,
    relative_path: Option<String>,
    query: Option<String>,
}
struct CancelOnDrop(Arc<AtomicBool>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}
async fn handle(
    State(state): State<AppState>,
    Path(operation): Path<String>,
    Json(request): Json<Request>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let cancel = CancelOnDrop(Arc::new(AtomicBool::new(false)));
    let cancelled = cancel.0.clone();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<Value> {
        let roots = state.db_manager.with_conn(|conn| -> anyhow::Result<_> {
            let database = conn
                .path()
                .ok_or_else(|| anyhow::anyhow!("Workspace database path is unavailable"))?;
            let private_root = std::path::Path::new(database)
                .parent()
                .ok_or_else(|| anyhow::anyhow!("Workspace database has no parent"))?
                .join("workspaces");
            files::roots(conn, &private_root, &request.scope)
        })??;
        if operation == "roots" {
            return Ok(json!({"roots": roots}));
        }
        if operation == "search" {
            return files::search(
                &roots,
                request.query.as_deref().unwrap_or_default(),
                &cancelled,
            );
        }
        let file = FileRef {
            root_id: request
                .root_id
                .ok_or_else(|| anyhow::anyhow!("Missing rootId"))?,
            relative_path: request.relative_path.unwrap_or_default(),
        };
        match operation.as_str() {
            "tree" => files::tree(&roots, &file),
            "read" => files::read(&roots, &file),
            "diff" => Ok(serde_json::to_value(files::diff(&roots, &file)?)?),
            _ => anyhow::bail!("Unknown workspace file operation"),
        }
    })
    .await;
    match result {
        Ok(Ok(value)) => Ok(Json(value)),
        Ok(Err(error)) => Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error":error.to_string()})),
        )),
        Err(error) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":error.to_string()})),
        )),
    }
}
pub fn routes() -> Router<AppState> {
    Router::new().route("/api/workspace-files/{operation}", post(handle))
}
