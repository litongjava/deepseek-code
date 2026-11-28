//! Persist the complete opaque Responses compact window without converting it to text.
use super::*;
use crate::native_engine::{
    openai_client::OpenAIMessage,
    provider_manager::{ApiFormat, ResolvedProvider},
    provider_runtime,
};
use rusqlite::OptionalExtension;
use std::time::Duration;

pub(crate) fn key(message: &str) -> String {
    format!("compaction.native.{message}")
}

pub(crate) fn load(conn: &rusqlite::Connection, message: &str) -> Result<Option<Value>> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM peripheral_settings WHERE key=?1",
            [key(message)],
            |r| r.get(0),
        )
        .optional()?;
    raw.map(|raw| serde_json::from_str(&raw).map_err(Into::into))
        .transpose()
}

pub(crate) fn supports(provider: &ResolvedProvider) -> bool {
    provider
        .model
        .id
        .rsplit('/')
        .next()
        .is_some_and(|id| id.starts_with("gpt-") || id.starts_with("chatgpt-"))
        && compact_transport(provider)
}

fn compact_transport(provider: &ResolvedProvider) -> bool {
    matches!(provider.provider.api_format, ApiFormat::OpenAI)
        && provider.provider.builtin_id.is_some()
        && provider_runtime::resolved_transport(provider)
            .is_ok_and(|(protocol, _)| protocol == "openai-responses")
}

pub(crate) fn validate_history(messages: &[Value], provider: &ResolvedProvider) -> Result<()> {
    for message in messages {
        if let Some(native) = message
            .get("provider_content")
            .filter(|value| value["compaction"] == true)
        {
            ensure!(compact_transport(provider)
                && native["provider"] == provider.provider.id
                && native["base_url"] == provider.provider.base_url.trim_end_matches('/')
                && native["model"] == provider.model.id,
                "This session contains an opaque Responses compact window for another provider/model. Switch back to its original provider/model or branch from before compaction; the original transcript is retained.");
        }
    }
    Ok(())
}

pub(crate) async fn generate(
    history: Vec<Value>,
    id: &str,
    provider: &ResolvedProvider,
    instructions: &str,
    root: Option<&std::path::Path>,
    options: crate::native_engine::request_options::RequestOptions,
) -> Result<GeneratedSummary> {
    let messages = history
        .into_iter()
        .map(serde_json::from_value::<OpenAIMessage>)
        .collect::<serde_json::Result<Vec<_>>>()?;
    generate_messages_with_options(
        provider,
        options.compaction("native"),
        messages,
        instructions,
        Some(id),
        root,
    )
    .await
}

#[cfg(test)]
pub(crate) async fn generate_messages(
    provider: &ResolvedProvider,
    session: Option<&str>,
    messages: Vec<OpenAIMessage>,
    instructions: &str,
) -> Result<GeneratedSummary> {
    generate_messages_with_options(
        provider,
        crate::native_engine::request_options::RequestOptions::for_session(session)
            .compaction("native"),
        messages,
        instructions,
        session,
        None,
    )
    .await
}

pub(crate) async fn generate_messages_with_options(
    provider: &ResolvedProvider,
    options: crate::native_engine::request_options::RequestOptions,
    messages: Vec<OpenAIMessage>,
    instructions: &str,
    session: Option<&str>,
    root: Option<&std::path::Path>,
) -> Result<GeneratedSummary> {
    let provider = &provider.current()?;
    let body = provider_runtime::compaction_request(provider, messages, instructions, session)?;
    let (url, headers) = provider_runtime::compact_endpoint(provider)?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .connect_timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let mut options = options;
    let mut metrics = crate::native_engine::compaction_metrics::CompactionMetrics::start(
        &mut options,
        provider,
        "openai-responses-compact",
    )?;
    let identity = crate::native_engine::request_identity::RequestIdentity::for_explicit(
        provider,
        "compaction",
        session,
        options
            .recording
            .as_ref()
            .and_then(|context| context.parent_session_id.as_deref()),
        options.model_request_id.as_deref(),
    );
    let request = client
        .post(url)
        .headers(headers)
        .headers(options.headers(provider)?)
        .headers(identity.headers())
        .json(&body);
    let bytes = crate::performance::retry_with_backoff(|| async {
        let attempt = request.try_clone().context("Cannot replay native compaction request")?;
        let response = crate::native_engine::request_recording::send(
            attempt, &options, provider, "openai-responses-compact", false,
        )
        .await?;
        let mut response = crate::performance::checked_response(response).await?;
        const LIMIT: usize = 32 * 1024 * 1024;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            ensure!(bytes.len() + chunk.len() <= LIMIT, "Native compaction response exceeds 32 MiB");
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    })
    .await
    .map_err(|error| {
        let message = format!("Native compaction gateway request failed: {error}");
        error.context(message)
    })?;
    let result: Value = serde_json::from_slice(&bytes).context("Invalid native compaction JSON")?;
    if let Some(metrics) = &mut metrics {
        metrics.observe_usage(&result["usage"]);
    }
    ensure!(
        result["object"] == "response.compaction",
        "Gateway did not return a native response.compaction result"
    );
    let items = result["output"]
        .as_array()
        .context("Native compaction output must be an array")?;
    ensure!(
        items.iter().any(|v| v["type"] == "compaction"
            && v["encrypted_content"]
                .as_str()
                .is_some_and(|s| !s.is_empty())),
        "Native compaction output has no encrypted compaction item"
    );
    let mut native = json!({"api":"openai-responses","compaction":true,"items":items,"usage":result["usage"],
            "provider":provider.provider.id,"base_url":provider.provider.base_url.trim_end_matches('/'),"model":provider.model.id});
    crate::native_engine::tool_loop::seal_native_window(
        &mut native,
        root,
        session.unwrap_or_default(),
    );
    if let Some(metrics) = metrics {
        metrics.finish(&result["usage"], "compact", "completed")?;
    }
    Ok(GeneratedSummary {
        text: "Native Responses compaction completed. The encrypted context is saved and will be restored for this provider/model. Earlier messages remain in session history.".into(),
        native:Some(native),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_compaction_commit_is_atomic_and_rejects_changed_history() {
        for conflict in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            let paths = crate::runtime::RuntimePaths::new(temp.path().join("data")).unwrap();
            let store = crate::runtime::SessionStore::new(paths.open_database().unwrap());
            let id = store.create(temp.path(), "model", "provider").unwrap();
            store.append(&id, "user", "Task").unwrap();
            store.append(&id, "assistant", "Pending").unwrap();
            let prepared = prepare(&store.db, &id).unwrap();
            if conflict {
                store.append(&id, "user", "New instruction").unwrap();
            } else {
                store.db.fixture_shard(&id, |conn| conn.execute_batch("CREATE TRIGGER fail_native BEFORE UPDATE ON compaction_state BEGIN SELECT RAISE(ABORT, 'failure'); END;")).unwrap().unwrap();
            }
            let before = store.history(&id).unwrap();
            let summary = GeneratedSummary {
                text: "Native context".into(),
                native: Some(json!({"items":[{"type":"compaction","encrypted_content":"opaque"}]})),
            };
            assert!(commit_with_context(&store.db, prepared, &summary, None).is_err());
            assert_eq!(store.history(&id).unwrap(), before);
            let count = store.db.fixture_shard(&id, |conn| conn.query_row("SELECT COUNT(*) FROM native_boundaries", [], |r| r.get::<_, i64>(0))).unwrap().unwrap();
            assert_eq!(count, 0);
        }
    }
}
