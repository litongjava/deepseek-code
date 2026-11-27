use super::super::AppState;
use axum::{
    routing::{get, post, put},
    Router,
};
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/memory/export", get(file_stats))
        .route("/api/memory/import", post(file_unsupported))
        .route("/api/memory/clear", post(file_unsupported))
        .route("/api/memory/search", post(file_search))
        .route("/api/memory/stats", get(file_stats))
        .route("/api/memory/ingest", post(file_ingest))
        .route(
            "/api/memory/items/{id}",
            put(file_update).delete(file_delete),
        )
        .route("/api/memory/config", get(get_config).put(set_config))
        .route("/api/memory/legacy/export", get(export))
        .route("/api/memory/legacy/import", post(import))
        .route(
            "/api/memory/legacy/search",
            post(super::super::legacy_search_handler),
        )
        .route(
            "/api/memory/legacy/ingest",
            post(super::super::legacy_ingest_handler),
        )
        .route(
            "/api/memory/legacy/stats",
            get(super::super::memory_stats_handler),
        )
        .route("/api/memory/legacy/config", get(get_config))
        .route("/api/memory/legacy/config", put(set_config))
        .route(
            "/api/memory/legacy/clear",
            post(super::super::memory_clear_handler),
        )
        .route(
            "/api/memory/legacy/items/{id}",
            put(update_item).delete(delete_item),
        )
}

#[derive(serde::Deserialize)]
struct UpdateItem {
    content: String,
    importance: f64,
}

fn item_result(
    result: anyhow::Result<bool>,
) -> Result<axum::Json<serde_json::Value>, (axum::http::StatusCode, axum::Json<serde_json::Value>)>
{
    match result {
        Ok(true) => Ok(axum::Json(serde_json::json!({"status": "ok"}))),
        Ok(false) => Err((
            axum::http::StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({"error": "Memory not found"})),
        )),
        Err(e) => Err(error(e)),
    }
}

async fn update_item(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::Json(request): axum::Json<UpdateItem>,
) -> Result<axum::Json<serde_json::Value>, (axum::http::StatusCode, axum::Json<serde_json::Value>)>
{
    item_result(
        state
            .memex_client
            .update(&id, &request.content, request.importance)
            .await,
    )
}

async fn delete_item(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> Result<axum::Json<serde_json::Value>, (axum::http::StatusCode, axum::Json<serde_json::Value>)>
{
    item_result(state.memex_client.delete(&id).await)
}

async fn export(
    axum::extract::State(state): axum::extract::State<AppState>,
) -> Result<
    axum::Json<crate::memory::MemoryArchive>,
    (axum::http::StatusCode, axum::Json<serde_json::Value>),
> {
    state
        .memex_client
        .export_archive()
        .await
        .map(axum::Json)
        .map_err(error)
}

#[derive(serde::Deserialize)]
struct ImportRequest {
    archive: serde_json::Value,
    preview: bool,
}

async fn import(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::Json(request): axum::Json<ImportRequest>,
) -> Result<
    axum::Json<crate::memory::MemoryImportReport>,
    (axum::http::StatusCode, axum::Json<serde_json::Value>),
> {
    state
        .memex_client
        .import_archive(request.archive, request.preview)
        .await
        .map(axum::Json)
        .map_err(error)
}

fn error(error: anyhow::Error) -> (axum::http::StatusCode, axum::Json<serde_json::Value>) {
    (
        axum::http::StatusCode::BAD_REQUEST,
        axum::Json(serde_json::json!({"error":error.to_string()})),
    )
}

// Scope is selected from trusted persisted project/session state, never a path.
use deepseek_code_core::services::memory_files::{FileMemory, Selection};
type FileResponse =
    Result<axum::Json<serde_json::Value>, (axum::http::StatusCode, axum::Json<serde_json::Value>)>;
fn context(state: &AppState) -> deepseek_code_core::services::ServiceContext {
    deepseek_code_core::services::ServiceContext {
        paths: deepseek_code_core::runtime::RuntimePaths {
            data_dir: (*state.data_dir).clone(),
        },
        db: state.db_manager.clone(),
        credentials: std::sync::Arc::new(deepseek_code_core::services::EnvironmentCredentials),
    }
}
async fn get_config(axum::extract::State(state): axum::extract::State<AppState>) -> FileResponse {
    Ok(axum::Json(serde_json::json!(
        deepseek_code_core::services::memory_files::config(&context(&state)).map_err(error)?
    )))
}
async fn set_config(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::Json(config): axum::Json<crate::memory::MemoryConfig>,
) -> FileResponse {
    deepseek_code_core::services::memory_files::save_config(&context(&state), &config)
        .map_err(error)?;
    Ok(axum::Json(serde_json::json!({"status":"updated"})))
}
fn file_service(state: &AppState, scope: &Selection) -> anyhow::Result<FileMemory> {
    FileMemory::select(&context(state), scope)
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FileQuery {
    scope: Selection,
    #[serde(default)]
    query: String,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FileWrite {
    scope: Selection,
    name: String,
    content: String,
    version: Option<String>,
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FileDelete {
    scope: Selection,
    version: String,
}
async fn file_search(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::Json(request): axum::Json<FileQuery>,
) -> FileResponse {
    file_service(&state, &request.scope)
        .and_then(|m| m.listing(&request.query))
        .map(axum::Json)
        .map_err(file_error)
}
async fn file_stats(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Query(scope): axum::extract::Query<Selection>,
) -> FileResponse {
    file_service(&state, &scope)
        .and_then(|m| m.listing(""))
        .map(axum::Json)
        .map_err(file_error)
}
async fn file_ingest(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::Json(request): axum::Json<FileWrite>,
) -> FileResponse {
    file_service(&state, &request.scope)
        .and_then(|m| m.put(&request.name, &request.content, request.version.as_deref()))
        .map(axum::Json)
        .map_err(file_error)
}
async fn file_update(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::Json(request): axum::Json<FileWrite>,
) -> FileResponse {
    if request.version.is_none() || request.name != id {
        return Err(error(anyhow::anyhow!(
            "Update requires matching name and content version"
        )));
    }
    file_service(&state, &request.scope)
        .and_then(|m| m.put(&id, &request.content, request.version.as_deref()))
        .map(axum::Json)
        .map_err(file_error)
}
async fn file_delete(
    axum::extract::State(state): axum::extract::State<AppState>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::Json(request): axum::Json<FileDelete>,
) -> FileResponse {
    file_service(&state, &request.scope)
        .and_then(|m| m.delete(&id, &request.version))
        .map(axum::Json)
        .map_err(file_error)
}
fn file_error(e: anyhow::Error) -> (axum::http::StatusCode, axum::Json<serde_json::Value>) {
    let conflict = matches!(
        e.downcast_ref::<deepseek_code_core::memory::files::MemoryFileError>(),
        Some(deepseek_code_core::memory::files::MemoryFileError::Conflict(_))
    );
    let mut result = error(e);
    if conflict {
        result.0 = axum::http::StatusCode::CONFLICT;
    }
    result
}

async fn file_unsupported() -> FileResponse {
    Err(error(anyhow::anyhow!("Global memory writes are disabled. Select a scope and write/delete individual versioned files. Legacy archives use /api/memory/legacy/import.")))
}

#[cfg(test)]
mod file_tests {
    use super::*;
    #[test]
    fn file_memory_api_requires_scope_and_delete_version() {
        assert!(serde_json::from_value::<FileWrite>(
            serde_json::json!({"content":"old unscoped","importance":0.7})
        )
        .is_err());
        assert!(serde_json::from_value::<FileDelete>(
            serde_json::json!({"scope":{"session_id":"session"}})
        )
        .is_err());
        assert!(serde_json::from_value::<FileQuery>(
            serde_json::json!({"scope":{"path":"/untrusted"},"query":""})
        )
        .is_err());
        let request=serde_json::from_value::<FileWrite>(serde_json::json!({"scope":{"session_id":"session"},"name":"test.md","content":"note","version":"hash"})).unwrap();
        assert_eq!(request.version.as_deref(), Some("hash"));
    }
    #[test]
    fn file_memory_api_conflict_is_http_409() {
        assert_eq!(
            file_error(
                deepseek_code_core::memory::files::MemoryFileError::Conflict("test.md".into())
                    .into()
            )
            .0,
            axum::http::StatusCode::CONFLICT
        );
    }
}
