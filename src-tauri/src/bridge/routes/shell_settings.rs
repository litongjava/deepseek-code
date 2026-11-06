use super::super::AppState;
use axum::{http::StatusCode, routing::get, Json, Router};
use deepseek_code_core::services::{shell_settings, wsl};
use serde_json::{json, Value};

type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;

fn failure(error: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error":format!("{error:#}")})),
    )
}

async fn read() -> ApiResult {
    let root = crate::commands::app_data_dir().map_err(failure)?;
    inspect(&root)
}

fn inspect(root: &std::path::Path) -> ApiResult {
    let mut status = shell_settings::inspect(root).map_err(failure)?;
    if status["settings"]["terminalDefault"] == "auto" {
        let program = if cfg!(windows) {
            "powershell.exe".into()
        } else {
            std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into())
        };
        status["terminal"] = json!({"program":program,"source":"detected"});
    }
    Ok(Json(status))
}

async fn save(Json(settings): Json<shell_settings::ShellSettings>) -> ApiResult {
    let root = crate::commands::app_data_dir().map_err(failure)?;
    shell_settings::save(&root, &settings).map_err(failure)?;
    inspect(&root)
}

async fn distributions() -> ApiResult {
    Ok(Json(
        json!({"distributions":wsl::distributions().await.map_err(failure)?}),
    ))
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/shell-settings", get(read).put(save))
        .route("/api/shell-settings/distributions", get(distributions))
}
