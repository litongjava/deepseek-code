use super::super::AppState;
use axum::{extract::Query, http::StatusCode, routing::get, Json, Router};
use deepseek_code_core::{
    native_engine::tool_loop::builtin_tool_catalog,
    prompt::{default_system_prompt, PromptProfile},
    services::customization::{self, Customization},
};
use serde::Deserialize;
use serde_json::{json, Value};

type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/customization", get(read).put(write))
}

/// Which half of the file a write replaces. A prompt save must never touch the
/// tool half, so the page cannot lose a legacy entry by saving a prompt first.
#[derive(Deserialize, Default)]
struct SaveParams {
    #[serde(default)]
    section: Option<String>,
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
    // The effective switches come from core so the settings page never keeps a
    // copy of the core list or of the legacy `disabled_tools` rule.
    let tools: Vec<Value> = builtin_tool_catalog()
        .into_iter()
        .map(|tool| {
            let loading = saved.loading(&tool.name);
            let default = customization::default_loading(&tool.name);
            let core = customization::is_core_tool(&tool.name);
            json!({
                "name": tool.name,
                "description": tool.description,
                "core": core,
                "default_enabled": default.enabled,
                "default_on_demand": default.on_demand,
                "enabled": loading.enabled,
                "on_demand": loading.on_demand,
            })
        })
        .collect();
    Ok(Json(json!({
        "customization": saved,
        "default_system_prompt": default_system_prompt(PromptProfile::Code),
        "builtin_tools": tools,
    })))
}

async fn write(Query(params): Query<SaveParams>, Json(body): Json<Customization>) -> ApiResult {
    let builtin: Vec<String> = builtin_tool_catalog().into_iter().map(|tool| tool.name).collect();
    let data = data_dir()?;
    let saved = match params.section.as_deref().unwrap_or("tools") {
        "prompts" => customization::save_prompt(&data, &body.system_prompt, &builtin),
        "tools" => customization::save_tools(&data, &body, &builtin),
        other => return Err(failure(StatusCode::BAD_REQUEST, format!("Unknown settings section: {other}"))),
    }
    .map_err(|error| failure(StatusCode::BAD_REQUEST, error))?;
    // The saved form is returned so the page and the file agree after a write.
    Ok(Json(json!({"status": "saved", "customization": saved})))
}
