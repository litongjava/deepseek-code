use super::super::AppState;
use crate::db::conversation_repo as conversations;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use deepseek_code_core::db::session_store::{ProjectEdit, Projects};
use serde::Deserialize;
use serde_json::{json, Value};

type ApiError = (StatusCode, Json<Value>);
type ApiResult = Result<Json<Value>, ApiError>;

fn invalid(error: impl std::fmt::Display) -> ApiError {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error": error.to_string()})),
    )
}

async fn database<T, F>(state: &AppState, operation: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce(&crate::db::session_store::Stores) -> anyhow::Result<T> + Send + 'static,
{
    let db = state.db_manager.clone();
    tokio::task::spawn_blocking(move || operation(&db))
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": e.to_string()})),
            )
        })?
        .map_err(invalid)
}

#[derive(Deserialize)]
struct ProjectInput {
    name: Option<String>,
    description: Option<String>,
    instructions: Option<String>,
    folders: Option<Vec<String>>,
    is_archived: Option<Value>,
}
impl From<ProjectInput> for ProjectEdit {
    fn from(input: ProjectInput) -> Self {
        Self {
            name: input.name.map(|s| s.trim().to_owned()),
            description: input.description,
            instructions: input.instructions,
            folders: input.folders,
            archived: input
                .is_archived
                .map(|v| v.as_bool().unwrap_or_else(|| v.as_i64().unwrap_or(0) != 0)),
        }
    }
}
async fn list(State(state): State<AppState>) -> ApiResult {
    let rows = database(&state, |db| db.projects()).await?;
    Ok(Json(json!({"projects":rows})))
}
async fn create(State(state): State<AppState>, Json(input): Json<ProjectInput>) -> ApiResult {
    let project = database(&state, move |db| {
        db.create_project(&uuid::Uuid::new_v4().to_string(), &input.into())
    })
    .await?;
    Ok(Json(json!(project)))
}
async fn detail(Path(id): Path<String>, State(state): State<AppState>) -> ApiResult {
    let (project, files, chats) = database(&state, move |db| db.project_detail(&id)).await?;
    let mut result = json!(project);
    result["conversations"] = json!(chats
        .into_iter()
        .map(|row| row.with_desktop_model_key())
        .collect::<Vec<_>>());
    result["files"] = json!(files);
    Ok(Json(result))
}
async fn update(
    Path(id): Path<String>,
    State(state): State<AppState>,
    Json(input): Json<ProjectInput>,
) -> ApiResult {
    let edit = ProjectEdit::from(input);
    if edit.name.as_deref() == Some("") {
        return Err(invalid("Project name is required"));
    }
    let project = database(&state, move |db| db.edit_project(&id, &edit)).await?;
    Ok(Json(json!(project)))
}
#[derive(Deserialize)]
struct TrustInput {
    folders: Vec<String>,
}
async fn trust(
    Path(id): Path<String>,
    State(state): State<AppState>,
    Json(input): Json<TrustInput>,
) -> ApiResult {
    let project = database(&state, move |db| db.trust_project(&id, &input.folders)).await?;
    Ok(Json(json!(project)))
}
async fn remove(Path(id): Path<String>, State(state): State<AppState>) -> ApiResult {
    database(&state, move |db| db.delete_project(&id)).await?;
    Ok(Json(json!({"ok":true})))
}
async fn chats(Path(id): Path<String>, State(state): State<AppState>) -> ApiResult {
    let chats = database(&state, move |db| db.project_conversations(&id)).await?;
    Ok(Json(json!({"conversations":chats})))
}
#[derive(Deserialize)]
struct ChatInput {
    title: Option<String>,
    model: Option<String>,
    #[serde(default)]
    workspace_mode: conversations::WorkspaceMode,
    branch: Option<String>,
    #[serde(default)]
    use_worktree: bool,
}

async fn git_branches(Path(id): Path<String>, State(state): State<AppState>) -> ApiResult {
    let result = database(&state, move |db| {
        let project = db.trusted_project(&id)?;
        let root = project
            .workspace_path
            .ok_or_else(|| anyhow::anyhow!("Project has no folder"))?;
        crate::worktree::session::branches(std::path::Path::new(&root))
    })
    .await?;
    Ok(Json(json!(result)))
}
async fn create_chat(
    Path(id): Path<String>,
    State(state): State<AppState>,
    Json(input): Json<ChatInput>,
) -> ApiResult {
    let chat = database(&state, move |db| {
        db.create_project_chat(
            &id,
            input.title.as_deref(),
            input.model.as_deref(),
            input.workspace_mode,
            input.branch.as_deref(),
            input.use_worktree,
        )
    })
    .await?;
    Ok(Json(json!(chat.with_desktop_model_key())))
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/projects", get(list).post(create))
        .route(
            "/api/projects/{id}",
            get(detail).patch(update).delete(remove),
        )
        .route("/api/projects/{id}/trust", post(trust))
        .route("/api/projects/{id}/git/branches", get(git_branches))
        .route(
            "/api/projects/{id}/conversations",
            get(chats).post(create_chat),
        )
}
