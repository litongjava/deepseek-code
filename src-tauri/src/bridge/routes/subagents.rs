use super::super::AppState;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::get,
    Json, Router,
};
#[cfg(test)]
use deepseek_code_core::services::subagents;
use serde::Deserialize;
use serde_json::{json, Value};

type ApiError = (StatusCode, Json<Value>);

fn internal(error: impl std::fmt::Display) -> ApiError {
    tracing::error!("Subagent history read failed: {error}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error":"Subagent history unavailable","status":"unavailable"})),
    )
}

#[derive(Default, Deserialize)]
struct ListQuery {
    tool_use_id: Option<String>,
}

#[derive(Deserialize)]
struct PageQuery {
    #[serde(default)]
    offset: usize,
    #[serde(default = "default_limit")]
    limit: usize,
}
fn default_limit() -> usize {
    100
}

async fn list(
    Path(parent): Path<String>,
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> Result<Json<Value>, ApiError> {
    let result = tokio::task::spawn_blocking(move || {
        state
            .db_manager
            .invocation_list(&parent, query.tool_use_id.as_deref())
    })
    .await
    .map_err(internal)?
    .map_err(internal)?;
    Ok(Json(result))
}

async fn transcript(
    Path((parent, child)): Path<(String, String)>,
    State(state): State<AppState>,
    Query(query): Query<PageQuery>,
) -> Result<Json<Value>, ApiError> {
    if query.limit == 0 || query.limit > 1000 {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"limit must be between 1 and 1000"})),
        ));
    }
    let result = tokio::task::spawn_blocking(move || {
        state
            .db_manager
            .invocation_page(&parent, &child, query.offset, query.limit)
    })
    .await
    .map_err(internal)?
    .map_err(internal)?;
    result.map(Json).ok_or_else(|| (StatusCode::NOT_FOUND, Json(json!({
        "error":"Subagent association unavailable (legacy or unrelated session)","status":"association_unavailable"
    }))))
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/conversations/{id}/subagents", get(list))
        .route("/api/conversations/{id}/subagents/{child}", get(transcript))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_history_pages_share_cursor_and_validate_persisted_parent() -> anyhow::Result<()> {
        use deepseek_code_core::db::{message_repo::ToolCallRow, session_store::Stores};
        let root = tempfile::tempdir()?;
        let stores = Stores::open(root.path())?;
        let parent = stores.create("parent", Default::default())?;
        let child = stores.create("child", Default::default())?;
        stores.create("other", Default::default())?;
        let invocation = subagents::Invocation {
            parent_session_id: parent.id().into(),
            parent_message_id: parent.write(|w| w.append("assistant", "launch"))?,
            tool_use_id: "call".into(),
            child_session_id: child.id().into(),
            attempt_id: "attempt".into(),
            mode: "foreground".into(),
            status: "running".into(),
            description: "same label".into(),
        };
        parent.write(|w| w.child_invocation(&child.address(), &invocation))?;
        let first_id = child.write(|w| {
            let mut first = String::new();
            for i in 0..125 {
                let message = w.append("assistant", &"x".repeat(9000))?;
                if i == 0 {
                    first = message;
                }
            }
            Ok(first)
        })?;
        let display = child.save_mcp(
            &first_id,
            "tool-0",
            &json!({"content":[{"type":"image","mimeType":"image/png","data":"iVBORw0KGgo="}]}),
            false,
        )?;
        child.write(|w| {
            for i in 0..36 {
                w.tool(
                    &ToolCallRow {
                        id: format!("{first_id}:tool-{i}"),
                        message_id: first_id.clone(),
                        name: "Read".into(),
                        input: Some("{}".into()),
                        output: if i == 0 {
                            Some(display.clone())
                        } else if i == 35 {
                            None
                        } else {
                            Some("full output".into())
                        },
                        is_error: false,
                        sort_order: i,
                    },
                    None,
                )?;
            }
            Ok(())
        })?;
        let first = stores.invocation_page("parent", "child", 0, 100)?.unwrap();
        let second = stores
            .invocation_page("parent", "child", 100, 100)?
            .unwrap();
        assert_eq!(first["messages"].as_array().unwrap().len(), 100);
        assert_eq!(second["messages"].as_array().unwrap().len(), 25);
        assert_eq!(
            first["messages"][0]["content"].as_str().unwrap().len(),
            9000
        );
        assert_eq!(
            first["messages"][0]["tool_calls"].as_array().unwrap().len(),
            36
        );
        assert_eq!(
            first["messages"][0]["tool_calls"][0]["mcp_result"]["content"][0]["source"]["data"],
            "iVBORw0KGgo="
        );
        assert_eq!(first["messages"][0]["tool_calls"][35]["status"], "running");
        assert_eq!(first["next_offset"], 100);
        assert!(second["next_offset"].is_null());
        assert_eq!(first["cursor"], second["cursor"]);
        assert_eq!(first["status"], "unknown");
        assert!(stores.invocation_page("other", "child", 0, 100)?.is_none());
        child.delete()?;
        stores
            .create("child", Default::default())?
            .write(|w| w.append("assistant", "replacement history"))?;
        let missing = stores.invocation_page("parent", "child", 0, 100)?.unwrap();
        assert_eq!(missing["status"], "missing");
        assert_eq!(missing["messages"], json!([]));
        assert_ne!(missing["cursor"], first["cursor"]);
        parent.delete()?;
        assert!(stores.invocation_page("parent", "child", 0, 100)?.is_none());
        Ok(())
    }
}
