//! Peripheral services (scheduled prompts, learning, team context, templates)
//! over HTTP, for clients without the Desktop window such as browsers.

use super::super::AppState;
use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use deepseek_code_core::services::{EnvironmentCredentials, ServiceContext};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Deserialize)]
struct Body {
    request: crate::commands::peripheral::Request,
}

async fn peripheral(State(state): State<AppState>, Json(body): Json<Body>) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let fail = |error: String| (StatusCode::BAD_REQUEST, Json(json!({ "error": error })));
    let context = ServiceContext {
        paths: deepseek_code_core::runtime::RuntimePaths::new(state.data_dir.as_ref().clone()).map_err(|e| fail(e.to_string()))?,
        db: state.db_manager.clone(),
        // Server deployments provide service credentials through the environment.
        credentials: Arc::new(EnvironmentCredentials),
    };
    crate::commands::peripheral::run(context, &state.peripheral_host, body.request)
        .await
        .map(Json)
        .map_err(|e| fail(e.to_string()))
}

pub fn routes() -> Router<AppState> {
    Router::new().route("/api/peripheral", post(peripheral))
}
