use super::super::AppState;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    routing::get,
    Json, Router,
};
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
    let root = crate::commands::app_data_dir().map_err(internal)?;
    let result = tokio::task::spawn_blocking(move || {
        state
            .db_manager
            .with_conn(|conn| -> anyhow::Result<Value> {
                let tx = conn.unchecked_transaction()?;
                let mut invocations = subagents::list_invocations(&tx, &parent, None)?;
                let mut seen = std::collections::HashSet::new();
                for invocation in invocations.iter_mut().rev() {
                    if seen.insert(invocation.child_session_id.clone()) {
                        invocation.status =
                            subagents::authoritative_status(&tx, &root, invocation)?;
                    }
                }
                invocations.retain(|invocation| {
                    query
                        .tool_use_id
                        .as_deref()
                        .is_none_or(|tool| invocation.tool_use_id == tool)
                });
                Ok(json!({"invocations": invocations}))
            })?
    })
    .await
    .map_err(internal)?
    .map_err(internal)?;
    Ok(Json(result))
}

fn snapshot(
    conn: &rusqlite::Connection,
    root: &std::path::Path,
    parent: &str,
    child: &str,
    offset: usize,
    limit: usize,
) -> anyhow::Result<Option<Value>> {
    let tx = conn.unchecked_transaction()?;
    let Some(mut invocation) = subagents::list_invocations(&tx, parent, None)?
        .into_iter()
        .rev()
        .find(|invocation| invocation.child_session_id == child)
    else {
        return Ok(None);
    };
    let parent_exists = crate::db::conversation_repo::get_conversation(&tx, parent)?.is_some();
    let child_exists = crate::db::conversation_repo::get_conversation(&tx, child)?.is_some();
    let status = if child_exists {
        subagents::authoritative_status(&tx, root, &invocation)?
    } else {
        "missing".into()
    };
    invocation.status = status.clone();
    let messages = super::conversations::enriched_messages(&tx, child)?;
    let mut result = json!({"invocation":invocation,"messages":messages,"status":status,"parent_exists":parent_exists});
    let cursor = subagents::snapshot_cursor(&result);
    let count = messages.len();
    let end = offset.saturating_add(limit).min(count);
    result["messages"] = json!(messages
        .into_iter()
        .skip(offset)
        .take(limit)
        .collect::<Vec<_>>());
    result["next_offset"] = if end < count { json!(end) } else { Value::Null };
    result["cursor"] = json!(cursor);
    Ok(Some(result))
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
    let root = crate::commands::app_data_dir().map_err(internal)?;
    let result = tokio::task::spawn_blocking(move || {
        state
            .db_manager
            .with_conn(|conn| snapshot(conn, &root, &parent, &child, query.offset, query.limit))?
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
    fn full_history_pages_share_cursor_and_validate_persisted_parent() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::db::schema::SCHEMA_SQL).unwrap();
        deepseek_code_core::services::migrate(&conn).unwrap();
        for id in ["parent", "child", "other"] {
            crate::db::conversation_repo::insert_conversation(
                &conn, id, None, None, None, None, None, false, false, false, "now", "now", 0,
            )
            .unwrap();
        }
        let invocation = subagents::Invocation {
            parent_session_id: "parent".into(),
            parent_message_id: "parent-message".into(),
            tool_use_id: "call".into(),
            child_session_id: "child".into(),
            attempt_id: "attempt".into(),
            mode: "foreground".into(),
            status: "running".into(),
            description: "same label".into(),
        };
        conn.execute(
            "INSERT INTO peripheral_settings(key,value) VALUES(?1,?2)",
            [
                invocation.key(),
                serde_json::to_string(&invocation).unwrap(),
            ],
        )
        .unwrap();
        for i in 0..125 {
            crate::db::message_repo::insert_message(
                &conn,
                &format!("message-{i}"),
                "child",
                "assistant",
                &"x".repeat(9000),
                Some("complete thinking"),
                "now",
                false,
                i,
            )
            .unwrap();
        }
        for i in 0..35 {
            crate::db::message_repo::insert_tool_call(
                &conn,
                &format!("message-0:tool-{i}"),
                "message-0",
                "Read",
                Some("{}"),
                Some("full output"),
                false,
                i,
            )
            .unwrap();
        }
        crate::db::message_repo::insert_tool_call(
            &conn,
            "message-0:pending",
            "message-0",
            "Read",
            Some("{}"),
            None,
            false,
            35,
        )
        .unwrap();
        conn.execute("INSERT INTO mcp_tool_outputs(conversation_id,tool_use_id,content_json,metadata_json,display_text,is_error) VALUES('child','message-0:tool-0','[{\"type\":\"image\",\"source\":{\"type\":\"base64\",\"media_type\":\"image/png\",\"data\":\"YWJj\"}}]','{}','full output',0)", []).unwrap();
        let root = std::path::Path::new(".");
        let first = snapshot(&conn, root, "parent", "child", 0, 100)
            .unwrap()
            .unwrap();
        let second = snapshot(&conn, root, "parent", "child", 100, 100)
            .unwrap()
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
            "YWJj"
        );
        assert_eq!(first["messages"][0]["tool_calls"][35]["status"], "running");
        assert_eq!(first["next_offset"], 100);
        assert!(second["next_offset"].is_null());
        assert_eq!(first["cursor"], second["cursor"]);
        assert_eq!(first["status"], "unknown");
        assert!(snapshot(&conn, root, "other", "child", 0, 100)
            .unwrap()
            .is_none());
        conn.execute("DELETE FROM conversations WHERE id='parent'", [])
            .unwrap();
        let orphan = snapshot(&conn, root, "parent", "child", 0, 100)
            .unwrap()
            .unwrap();
        assert_eq!(orphan["parent_exists"], false);
        assert_eq!(orphan["messages"].as_array().unwrap().len(), 100);
        assert_ne!(orphan["cursor"], first["cursor"]);
        conn.execute("DELETE FROM messages WHERE conversation_id='child'", [])
            .unwrap();
        conn.execute("DELETE FROM conversations WHERE id='child'", [])
            .unwrap();
        assert_eq!(
            snapshot(&conn, root, "parent", "child", 0, 100)
                .unwrap()
                .unwrap()["status"],
            "missing"
        );
    }
}
