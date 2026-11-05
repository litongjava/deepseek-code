use super::super::AppState;
use axum::{http::StatusCode, routing::get, Json, Router};
use deepseek_code_core::{
    native_engine::tool_loop::builtin_tool_catalog,
    prompt::{default_system_prompt, PromptProfile},
    services::customization::{self, Customization},
};
use serde_json::{json, Value};

type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/customization", get(read).put(write))
}

fn failure(status: StatusCode, error: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    (status, Json(json!({"error": format!("{error:#}")})))
}

fn data_dir() -> Result<std::path::PathBuf, (StatusCode, Json<Value>)> {
    crate::commands::app_data_dir()
        .map(std::path::PathBuf::from)
        .map_err(|error| failure(StatusCode::INTERNAL_SERVER_ERROR, error))
}

async fn read() -> ApiResult {
    let saved = customization::load(&data_dir()?).map_err(|error| failure(StatusCode::INTERNAL_SERVER_ERROR, error))?;
    let tools: Vec<Value> = builtin_tool_catalog()
        .into_iter()
        .map(|tool| json!({"name": tool.name, "description": tool.description}))
        .collect();
    Ok(Json(json!({
        "customization": saved,
        "default_system_prompt": default_system_prompt(PromptProfile::Code),
        "builtin_tools": tools,
    })))
}

async fn write(Json(body): Json<Customization>) -> ApiResult {
    let builtin: Vec<String> = builtin_tool_catalog().into_iter().map(|tool| tool.name).collect();
    customization::save(&data_dir()?, &body, &builtin).map_err(|error| failure(StatusCode::BAD_REQUEST, error))?;
    Ok(Json(json!({"status": "saved"})))
}
