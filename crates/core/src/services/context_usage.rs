//! Local inspection of the last parent-session request; never prepares a new request.
use crate::{db::DbManager, native_engine::request_context::ContextUsage};
use anyhow::Result;
use serde_json::Value;

#[derive(Debug)]
pub struct LastRequest {
    pub model: String,
    pub started_at_ms: Option<i64>,
    pub status: String,
    pub usage: Option<ContextUsage>,
    pub reported_input_tokens: Option<u64>,
}

pub fn last_request(db: &DbManager, session: &str) -> Result<Option<LastRequest>> {
    let snapshot = db.with_conn(|conn| -> Result<Option<Value>> {
        let mut statement = conn.prepare(
            "SELECT t.requests_json FROM message_trajectory t JOIN messages m ON m.id=t.message_id WHERE m.conversation_id=?1 ORDER BY m.sort_order DESC, m.created_at DESC",
        )?;
        let mut rows = statement.query([session])?;
        while let Some(row) = rows.next()? {
            let raw: String = row.get(0)?;
            let Ok(snapshots) = serde_json::from_str::<Value>(&raw) else {
                // A corrupt row may hide a newer request. Do not report stale data.
                return Ok(None);
            };
            let Some(items) = snapshots.as_array() else { return Ok(None); };
            if let Some(snapshot) = items.iter().rev().find(|s| s["source"] == "native_engine" || s.get("source").is_none()) {
                // Preserve unknown metadata on this request; never replace it
                // with an older request merely because the old one has counts.
                return Ok(Some(snapshot.clone()));
            }
            // Local workflows and failed pre-request turns can persist [].
            // A row containing only side-query snapshots is not a parent request.
        }
        Ok(None)
    })??;
    let Some(snapshot) = snapshot else {
        return Ok(None);
    };
    let usage = serde_json::from_value::<ContextUsage>(snapshot["contextUsage"].clone())
        .ok()
        .filter(|u| u.version == 1);
    // Anthropic input excludes cached input; normalized OpenAI input includes it.
    let raw_usage = &snapshot["usage"];
    let reported_input_tokens = raw_usage["prompt_tokens"].as_u64().or_else(|| {
        raw_usage["input_tokens"].as_u64().map(|input| {
            input
                .saturating_add(raw_usage["cache_read_input_tokens"].as_u64().unwrap_or(0))
                .saturating_add(
                    raw_usage["cache_creation_input_tokens"]
                        .as_u64()
                        .unwrap_or(0),
                )
        })
    });
    Ok(Some(LastRequest {
        model: snapshot["model"]
            .as_str()
            .unwrap_or("unknown")
            .chars()
            .filter(|c| !c.is_control())
            .take(200)
            .collect(),
        started_at_ms: snapshot["startedAt"].as_i64(),
        status: match snapshot["status"].as_str() {
            Some("completed") => "completed",
            Some("error") => "error",
            Some("running") => "incomplete",
            Some("cancelled") => "cancelled",
            _ => "unknown",
        }
        .into(),
        usage,
        reported_input_tokens,
    }))
}

/// Current context occupancy: an estimate of the persisted history after the
/// latest compaction boundary, plus the instructions and tool definitions
/// measured on the last parent request. Returns `None` for an unknown session.
pub fn estimated_tokens(
    store: &crate::runtime::SessionStore,
    session: &str,
) -> Result<Option<u64>> {
    let exists = store
        .db
        .with_conn(|conn| crate::db::conversation_repo::get_conversation(conn, session))??
        .is_some();
    if !exists {
        return Ok(None);
    }
    // The same transport estimator the engine records for each request.
    let history = store.model_history(
        session,
        &crate::native_engine::provider_manager::ApiFormat::OpenAI,
    )?;
    let body = serde_json::json!({ "messages": history });
    let current = ContextUsage::from_body("openai-completions", &body, None, 0);
    let overhead = last_request(&store.db, session)?
        .and_then(|request| request.usage)
        .map_or(0, |usage| {
            usage
                .instructions
                .estimated_tokens
                .saturating_add(usage.tools.estimated_tokens)
                .saturating_add(usage.output_schema.estimated_tokens)
        });
    Ok(Some(
        current
            .history
            .estimated_tokens
            .saturating_add(current.instructions.estimated_tokens)
            .saturating_add(overhead),
    ))
}

/// The configured window of the selected model, else the engine's fallback table.
pub async fn context_limit(
    providers: &crate::native_engine::provider_manager::ProviderManager,
    provider: Option<&str>,
    model: &str,
) -> u64 {
    let resolved = match provider.filter(|id| !id.is_empty()) {
        Some(id) => providers.resolve_selected(id, model),
        None => providers.resolve_provider(model).await,
    };
    resolved
        .and_then(|resolved| resolved.model.context_window)
        .filter(|window| *window > 0)
        .map(u64::from)
        .unwrap_or_else(|| {
            crate::native_engine::token_counter::TokenCounter::get_model_context_limit(model) as u64
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        db::message_repo,
        runtime::{RuntimePaths, SessionStore},
    };
    use serde_json::json;

    #[test]
    fn last_request_is_session_local_and_missing_metadata_stays_unknown() {
        let temp = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
        let store = SessionStore::new(paths.open_database().unwrap());
        let parent = store.create(temp.path(), "model", "provider").unwrap();
        assert!(last_request(&store.db, &parent).unwrap().is_none());
        let child = store.create(temp.path(), "child", "provider").unwrap();
        for (session, id, model, usage) in [
            (
                &parent,
                "p",
                "model",
                json!({"input_tokens":10,"cache_read_input_tokens":20,"cache_creation_input_tokens":30}),
            ),
            (
                &child,
                "c",
                "child",
                json!({"prompt_tokens":100,"prompt_tokens_details":{"cached_tokens":40}}),
            ),
        ] {
            let message = store.append(session, "assistant", "private body").unwrap();
            store.db.with_conn(|conn| message_repo::save_request_snapshots(conn, &message, &json!([{"id":id,"source":"native_engine","model":model,"usage":usage,"status":"completed"}]))).unwrap().unwrap();
        }
        let result = last_request(&store.db, &parent).unwrap().unwrap();
        assert_eq!(result.model, "model");
        assert_eq!(result.reported_input_tokens, Some(60));
        assert!(result.usage.is_none());
        assert_eq!(
            last_request(&store.db, &child)
                .unwrap()
                .unwrap()
                .reported_input_tokens,
            Some(100)
        );
        for snapshots in [
            json!([]),
            json!([{"id":"side","source":"web_search","model":"side-query-model"}]),
        ] {
            let message = store
                .append(&parent, "assistant", "local workflow output")
                .unwrap();
            store
                .db
                .with_conn(|conn| message_repo::save_request_snapshots(conn, &message, &snapshots))
                .unwrap()
                .unwrap();
            let prior = last_request(&store.db, &parent).unwrap().unwrap();
            assert_eq!(prior.model, "model");
            assert_eq!(prior.reported_input_tokens, Some(60));
        }
        let message = store.append(&parent, "assistant", "new response").unwrap();
        store.db.with_conn(|conn| message_repo::save_request_snapshots(conn, &message, &json!([{"id":"new","source":"native_engine","model":"new-model","contextUsage":{"bad":"PRIVATE"}}]))).unwrap().unwrap();
        let result = last_request(&store.db, &parent).unwrap().unwrap();
        assert_eq!(result.model, "new-model");
        assert!(result.usage.is_none());
        assert!(result.reported_input_tokens.is_none());
        store
            .db
            .with_conn(|conn| {
                conn.execute("DELETE FROM messages WHERE conversation_id=?1", [&parent])
            })
            .unwrap()
            .unwrap();
        assert!(last_request(&store.db, &parent).unwrap().is_none());
    }

    #[tokio::test]
    async fn context_size_follows_compaction_and_measured_overhead() {
        let temp = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
        let store = SessionStore::new(paths.open_database().unwrap());
        assert_eq!(estimated_tokens(&store, "missing-session").unwrap(), None);
        let session = store
            .create(temp.path(), "fixture-model", "fixture")
            .unwrap();
        let empty = estimated_tokens(&store, &session).unwrap().unwrap();
        store.append(&session, "user", &"x".repeat(3000)).unwrap();
        let answer = store
            .append(&session, "assistant", &"y".repeat(3000))
            .unwrap();
        let full = estimated_tokens(&store, &session).unwrap().unwrap();
        assert!(full >= empty + 2000, "history must be counted: {full}");
        let estimate = |tokens: u64| json!({"serialized_utf8_bytes":tokens * 3,"estimated_tokens":tokens,"unknown_parts":0});
        let usage = json!({"version":1,"history":estimate(1),"instructions":estimate(500),"tools":estimate(700),"output_schema":estimate(0),
            "configured_context_window":null,"requested_output_budget":0,"sent_output_reserve":null,"estimated_input_tokens":null});
        store.db.with_conn(|conn| message_repo::save_request_snapshots(conn, &answer, &json!([{"id":"r","source":"native_engine","model":"fixture-model","contextUsage":usage}]))).unwrap().unwrap();
        assert_eq!(
            estimated_tokens(&store, &session).unwrap().unwrap(),
            full + 1200
        );
        store
            .db
            .with_conn(|conn| -> anyhow::Result<()> {
                message_repo::insert_message(
                    conn, "boundary", &session, "user", "summary", None, "now", true, 10,
                )?;
                Ok(())
            })
            .unwrap()
            .unwrap();
        let compacted = estimated_tokens(&store, &session).unwrap().unwrap();
        assert!(
            compacted < full,
            "compaction must shrink the estimate: {compacted} < {full}"
        );
    }

    #[tokio::test]
    async fn context_limit_prefers_configured_window_then_engine_fallback() {
        use crate::native_engine::provider_manager::{Provider, ProviderManager};
        let provider: Provider = serde_json::from_value(json!({"id":"fixture","name":"fixture","base_url":"http://127.0.0.1:9","api_key":"test","api_format":"openai","enabled":true,
            "models":[{"id":"fixture-model","name":"fixture","enabled":true,"max_tokens":1000,"context_window":64000,"supports_vision":false,"supports_web_search":false}]})).unwrap();
        let providers = ProviderManager::in_memory(vec![provider], std::path::PathBuf::new());
        assert_eq!(
            context_limit(&providers, Some("fixture"), "fixture-model").await,
            64000
        );
        assert_eq!(
            context_limit(&providers, None, "fixture::fixture-model").await,
            64000
        );
        assert_eq!(
            context_limit(&providers, None, "deepseek-chat").await,
            128000
        );
        assert_eq!(
            context_limit(&providers, Some("absent"), "unknown-model").await,
            80000
        );
    }
}
