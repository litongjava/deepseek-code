//! Local inspection of the last parent-session request; never prepares a new request.
use crate::{
    db::{message_repo, DbManager},
    native_engine::request_context::ContextUsage,
};
use anyhow::Result;
use serde_json::Value;

#[derive(Debug)]
pub struct LastRequest {
    pub model: String,
    pub provider: Option<String>,
    pub started_at_ms: Option<i64>,
    pub status: String,
    pub usage: Option<ContextUsage>,
    pub reported_input_tokens: Option<u64>,
}

/// The compaction boundary a request actually used, and when it committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Boundary {
    pub id: String,
    pub created_at_ms: Option<i64>,
}

pub fn last_request(db: &DbManager, session: &str) -> Result<Option<LastRequest>> {
    matching_request(db, session, |_| true)
}

fn same_boundary(snapshot: &Value, boundary: Option<&str>) -> bool {
    match boundary {
        Some(id) => snapshot["contextBoundary"].as_str() == Some(id),
        // A transcript with no boundary: a request that recorded one was built
        // from a context this session no longer has.
        None => snapshot
            .get("contextBoundary")
            .is_none_or(|value| value.is_null()),
    }
}

fn same_configuration(snapshot: &Value, model: &str, provider: Option<&str>) -> bool {
    if snapshot["model"].as_str() != Some(model) {
        return false;
    }
    match provider {
        Some(provider) => snapshot["providerId"].as_str() == Some(provider),
        None => true,
    }
}

/// Last parent request built from exactly this context: the compaction
/// boundary it recorded must match the session's latest one, and its model and
/// provider must match the current selection. Timestamps are not used as
/// proof, so a request that started before a boundary in the same millisecond
/// is never mistaken for one that used the new context.
pub fn last_request_for(
    db: &DbManager,
    session: &str,
    boundary: Option<&str>,
    model: &str,
    provider: Option<&str>,
) -> Result<Option<LastRequest>> {
    matching_request(db, session, |snapshot| {
        same_boundary(snapshot, boundary) && same_configuration(snapshot, model, provider)
    })
}

/// Last parent request that recorded this exact boundary and configuration.
/// Kept for callers that only know the boundary id.
pub fn last_request_since(
    db: &DbManager,
    session: &str,
    boundary_id: &str,
) -> Result<Option<LastRequest>> {
    matching_request(db, session, |snapshot| {
        snapshot["contextBoundary"].as_str() == Some(boundary_id)
    })
}

/// The latest successful compaction boundary, with its id and commit time.
pub fn latest_boundary(db: &DbManager, session: &str) -> Result<Option<Boundary>> {
    db.with_conn(|conn| -> Result<Option<Boundary>> {
        use rusqlite::OptionalExtension;
        let row: Option<(String, String)> = conn
            .query_row(
                "SELECT id, created_at FROM messages WHERE conversation_id=?1 AND is_compact_boundary=1 ORDER BY sort_order DESC LIMIT 1",
                [session],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(row.map(|(id, created)| Boundary {
            id,
            created_at_ms: chrono::DateTime::parse_from_rfc3339(&created)
                .ok()
                .map(|at| at.timestamp_millis()),
        }))
    })?
}

/// When the latest successful compaction boundary was committed, in
/// milliseconds since the epoch. `None` before the first compaction.
pub fn latest_boundary_ms(db: &DbManager, session: &str) -> Result<Option<i64>> {
    Ok(latest_boundary(db, session)?.and_then(|boundary| boundary.created_at_ms))
}

/// Last measured parent input, for a clearly labelled historical status fallback.
pub fn last_reported_request(
    db: &DbManager,
    session: &str,
    model: &str,
) -> Result<Option<LastRequest>> {
    matching_request(db, session, |snapshot| {
        snapshot["model"] == model
            && (snapshot["usage"]["prompt_tokens"].is_u64()
                || snapshot["usage"]["input_tokens"].is_u64())
    })
}

fn matching_request(
    db: &DbManager,
    session: &str,
    accept: impl Fn(&Value) -> bool,
) -> Result<Option<LastRequest>> {
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
            if let Some(snapshot) = items.iter().rev().find(|s| (s["source"] == "native_engine" || s.get("source").is_none()) && accept(s)) {
                // Preserve unknown metadata on this request; never replace it
                // with an older request merely because the old one has counts.
                // Only the selected snapshot is expanded: restoring every row
                // would multiply the status query's work.
                let mut snapshot = snapshot.clone();
                message_repo::load_trajectory_blobs(conn, &mut snapshot)?;
                return Ok(Some(snapshot));
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
        provider: snapshot["providerId"].as_str().map(|provider| {
            provider
                .chars()
                .filter(|c| !c.is_control())
                .take(200)
                .collect()
        }),
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

/// A local estimate of the current context, and whether every part was
/// measurable. Images, opaque Responses state and an unknown instruction/tool
/// overhead make it partial. `unknown_parts` counts the pieces that could not
/// be measured, so a partial estimate is never shown as a percentage.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Estimate {
    pub tokens: u64,
    pub complete: bool,
    pub unknown_parts: u64,
}

fn estimate_history(
    store: &crate::runtime::SessionStore,
    session: &str,
    model: &str,
    provider: Option<&str>,
) -> Result<Option<Estimate>> {
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
    let opaque = history
        .iter()
        .any(|message| message.get("provider_content").is_some());
    let body = serde_json::json!({ "messages": history });
    let current = ContextUsage::from_body("openai-completions", &body, None, 0);
    // The prompt and tool definitions are not part of the transcript. Reuse the
    // overhead measured on a request with the current model and provider; a
    // different configuration makes those numbers unusable.
    let overhead = matching_request(&store.db, session, |snapshot| {
        same_configuration(snapshot, model, provider)
    })?
    .and_then(|request| request.usage)
    .map(|usage| {
        (
            usage
                .instructions
                .estimated_tokens
                .saturating_add(usage.tools.estimated_tokens)
                .saturating_add(usage.output_schema.estimated_tokens),
            usage
                .instructions
                .unknown_parts
                .saturating_add(usage.tools.unknown_parts)
                .saturating_add(usage.output_schema.unknown_parts),
        )
    });
    let history_tokens = current
        .history
        .estimated_tokens
        .saturating_add(current.instructions.estimated_tokens);
    let history_unknown = current.unknown_parts();
    let (overhead_tokens, overhead_unknown) = match overhead {
        Some((tokens, unknown)) => (tokens, unknown),
        // Without a measured overhead the prompt and tool definitions are one
        // unknown piece; do not present the result as a complete occupancy.
        None => (0, 3),
    };
    Ok(Some(Estimate {
        tokens: history_tokens.saturating_add(overhead_tokens),
        complete: !opaque && history_unknown == 0 && overhead_unknown == 0,
        unknown_parts: history_unknown
            .saturating_add(overhead_unknown)
            .saturating_add(u64::from(opaque)),
    }))
}

/// Current context occupancy: an estimate of the persisted history after the
/// latest compaction boundary, plus the instructions and tool definitions
/// measured on a request with the current configuration. Returns `None` for an
/// unknown session.
pub fn estimated_tokens(
    store: &crate::runtime::SessionStore,
    session: &str,
    model: &str,
    provider: Option<&str>,
) -> Result<Option<u64>> {
    Ok(estimate_history(store, session, model, provider)?.map(|estimate| estimate.tokens))
}

/// Occupancy of the context that the latest compaction installed. `None` when
/// the session has no boundary, so the caller keeps the request-based display.
pub fn estimated_after_compaction(
    store: &crate::runtime::SessionStore,
    session: &str,
    model: &str,
    provider: Option<&str>,
) -> Result<Option<Estimate>> {
    if latest_boundary(&store.db, session)?.is_none() {
        return Ok(None);
    }
    estimate_history(store, session, model, provider)
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
        assert_eq!(
            last_reported_request(&store.db, &parent, "model")
                .unwrap()
                .unwrap()
                .reported_input_tokens,
            Some(60)
        );
        assert!(last_reported_request(&store.db, &parent, "child")
            .unwrap()
            .is_none());
        assert!(last_reported_request(&store.db, &child, "model")
            .unwrap()
            .is_none());
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
        assert_eq!(
            estimated_tokens(&store, "missing-session", "fixture-model", Some("fixture")).unwrap(),
            None
        );
        let session = store
            .create(temp.path(), "fixture-model", "fixture")
            .unwrap();
        let empty = estimated_tokens(&store, &session, "fixture-model", Some("fixture"))
            .unwrap()
            .unwrap();
        store.append(&session, "user", &"x".repeat(3000)).unwrap();
        let answer = store
            .append(&session, "assistant", &"y".repeat(3000))
            .unwrap();
        let full = estimated_tokens(&store, &session, "fixture-model", Some("fixture"))
            .unwrap()
            .unwrap();
        assert!(full >= empty + 2000, "history must be counted: {full}");
        let estimate = |tokens: u64| json!({"serialized_utf8_bytes":tokens * 3,"estimated_tokens":tokens,"unknown_parts":0});
        let usage = json!({"version":1,"history":estimate(1),"instructions":estimate(500),"tools":estimate(700),"output_schema":estimate(0),
            "configured_context_window":null,"requested_output_budget":0,"sent_output_reserve":null,"estimated_input_tokens":null});
        store.db.with_conn(|conn| message_repo::save_request_snapshots(conn, &answer, &json!([{"id":"r","source":"native_engine","providerId":"fixture","model":"fixture-model","contextUsage":usage}]))).unwrap().unwrap();
        assert_eq!(
            estimated_tokens(&store, &session, "fixture-model", Some("fixture"))
                .unwrap()
                .unwrap(),
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
        let compacted = estimated_tokens(&store, &session, "fixture-model", Some("fixture"))
            .unwrap()
            .unwrap();
        assert!(
            compacted < full,
            "compaction must shrink the estimate: {compacted} < {full}"
        );
    }

    #[tokio::test]
    async fn a_compacted_session_estimates_after_the_boundary_not_from_the_old_request() {
        let temp = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
        let store = SessionStore::new(paths.open_database().unwrap());
        let session = store
            .create(temp.path(), "fixture-model", "fixture")
            .unwrap();
        store.append(&session, "user", &"x".repeat(4000)).unwrap();
        let answer = store
            .append(&session, "assistant", &"y".repeat(4000))
            .unwrap();
        let estimate = |tokens: u64| json!({"serialized_utf8_bytes":tokens * 3,"estimated_tokens":tokens,"unknown_parts":0});
        let usage = json!({"version":1,"history":estimate(1),"instructions":estimate(500),"tools":estimate(700),"output_schema":estimate(0),
            "configured_context_window":null,"requested_output_budget":0,"sent_output_reserve":null,"estimated_input_tokens":null});
        store.db.with_conn(|conn| message_repo::save_request_snapshots(conn, &answer, &json!([{"id":"before","source":"native_engine","providerId":"fixture","model":"fixture-model","startedAt":1000,"status":"completed","inputTokens":90000,"usage":{"input_tokens":90000},"contextUsage":usage}]))).unwrap().unwrap();
        assert_eq!(
            last_request(&store.db, &session)
                .unwrap()
                .unwrap()
                .reported_input_tokens,
            Some(90000)
        );
        let prepared = crate::services::compaction::prepare(&store.db, &session).unwrap();
        crate::services::compaction::commit(&store.db, prepared, "short summary").unwrap();
        let boundary = latest_boundary(&store.db, &session).unwrap().unwrap();
        assert!(boundary.created_at_ms.unwrap_or(0) > 1000, "{boundary:?}");
        // The pre-boundary request never recorded this boundary's id, so a
        // request persisted later cannot claim to have used the new context.
        assert!(
            last_request_since(&store.db, &session, &boundary.id)
                .unwrap()
                .is_none(),
            "a request built from the previous context is not current"
        );
        let estimate =
            estimated_after_compaction(&store, &session, "fixture-model", Some("fixture"))
                .unwrap()
                .expect("a boundary has an estimate");
        assert!(estimate.complete, "{estimate:?}");
        assert!(estimate.tokens < 90000, "{estimate:?}");
        // A request that recorded the new boundary restores measured usage.
        let message = store.append(&session, "assistant", "after").unwrap();
        store.db.with_conn(|conn| message_repo::save_request_snapshots(conn, &message, &json!([{"id":"after","source":"native_engine","providerId":"fixture","contextBoundary":boundary.id,"model":"fixture-model","startedAt":boundary.created_at_ms.map(|ms|ms+1),"status":"completed","inputTokens":48000,"usage":{"input_tokens":48000}}]))).unwrap().unwrap();
        let after = last_request_since(&store.db, &session, &boundary.id)
            .unwrap()
            .unwrap();
        assert_eq!(after.reported_input_tokens, Some(48000));
    }

    #[tokio::test]
    async fn unknown_prompt_overhead_is_never_reported_as_complete() {
        let temp = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
        let store = SessionStore::new(paths.open_database().unwrap());
        let session = store
            .create(temp.path(), "fixture-model", "fixture")
            .unwrap();
        store.append(&session, "user", "Task").unwrap();
        let answer = store.append(&session, "assistant", "Result").unwrap();
        let part = |tokens: u64, unknown: u64| json!({"serialized_utf8_bytes":tokens * 3,"estimated_tokens":tokens,"unknown_parts":unknown});
        // Only the instruction overhead is unmeasurable.
        let usage = json!({"version":1,"history":part(1,0),"instructions":part(100,1),"tools":part(700,0),"output_schema":part(0,0),
            "configured_context_window":128000,"requested_output_budget":1000,"sent_output_reserve":1000,"estimated_input_tokens":null});
        store
            .db
            .with_conn(|conn| {
                message_repo::save_request_snapshots(
                    conn,
                    &answer,
                    &json!([{"id":"r","source":"native_engine","providerId":"fixture","model":"fixture-model","contextUsage":usage}]),
                )
            })
            .unwrap()
            .unwrap();
        let prepared = crate::services::compaction::prepare(&store.db, &session).unwrap();
        crate::services::compaction::commit(&store.db, prepared, "summary").unwrap();
        let estimate =
            estimated_after_compaction(&store, &session, "fixture-model", Some("fixture"))
                .unwrap()
                .unwrap();
        assert!(
            !estimate.complete,
            "unknown overhead is not a complete estimate: {estimate:?}"
        );
        assert!(estimate.unknown_parts > 0, "{estimate:?}");
    }

    #[tokio::test]
    async fn a_request_from_another_configuration_cannot_supply_overhead() {
        let temp = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
        let store = SessionStore::new(paths.open_database().unwrap());
        let session = store
            .create(temp.path(), "fixture-model", "fixture")
            .unwrap();
        store.append(&session, "user", "Task").unwrap();
        let answer = store.append(&session, "assistant", "Result").unwrap();
        let part = |tokens: u64| json!({"serialized_utf8_bytes":tokens * 3,"estimated_tokens":tokens,"unknown_parts":0});
        let usage = json!({"version":1,"history":part(1),"instructions":part(100),"tools":part(700),"output_schema":part(0),
            "configured_context_window":128000,"requested_output_budget":1000,"sent_output_reserve":1000,"estimated_input_tokens":null});
        store
            .db
            .with_conn(|conn| {
                message_repo::save_request_snapshots(
                    conn,
                    &answer,
                    &json!([{"id":"r","source":"native_engine","providerId":"other","model":"fixture-model","contextUsage":usage}]),
                )
            })
            .unwrap()
            .unwrap();
        let prepared = crate::services::compaction::prepare(&store.db, &session).unwrap();
        crate::services::compaction::commit(&store.db, prepared, "summary").unwrap();
        let estimate =
            estimated_after_compaction(&store, &session, "fixture-model", Some("fixture"))
                .unwrap()
                .unwrap();
        assert!(
            !estimate.complete,
            "another provider's overhead is not measurable here: {estimate:?}"
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
