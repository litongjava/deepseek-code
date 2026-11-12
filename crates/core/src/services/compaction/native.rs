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
    matches!(provider.provider.api_format, ApiFormat::OpenAI)
        && provider.provider.builtin_id.is_some()
        && provider.model.api.as_deref() == Some("openai-responses")
}

pub(crate) fn validate_history(messages: &[Value], provider: &ResolvedProvider) -> Result<()> {
    for message in messages {
        if let Some(native) = message
            .get("provider_content")
            .filter(|value| value["compaction"] == true)
        {
            ensure!(supports(provider)
                && native["provider"] == provider.provider.id
                && native["base_url"] == provider.provider.base_url.trim_end_matches('/')
                && native["model"] == provider.model.id,
                "This session contains an opaque Responses compact window for another provider/model. Switch back to its original provider/model or branch from before compaction; the original transcript is retained.");
        }
    }
    Ok(())
}

pub(crate) async fn generate(
    db: &std::sync::Arc<DbManager>,
    id: &str,
    provider: &ResolvedProvider,
    instructions: &str,
) -> Result<GeneratedSummary> {
    let history =
        crate::runtime::SessionStore::new(db.clone()).model_history(id, &ApiFormat::OpenAI)?;
    validate_history(&history, provider)?;
    let messages = history
        .into_iter()
        .map(serde_json::from_value::<OpenAIMessage>)
        .collect::<serde_json::Result<Vec<_>>>()?;
    generate_messages(provider, Some(id), messages, instructions).await
}

pub(crate) async fn generate_messages(
    provider: &ResolvedProvider,
    session: Option<&str>,
    messages: Vec<OpenAIMessage>,
    instructions: &str,
) -> Result<GeneratedSummary> {
    let body = provider_runtime::compaction_request(provider, messages, instructions)?;
    let mut url = reqwest::Url::parse(provider.provider.base_url.trim_end_matches('/'))
        .context("Native compaction requires a configured gateway URL")?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "Invalid native compaction gateway URL"
    );
    let root = url.path().trim_end_matches('/');
    let root = root.strip_suffix("/responses").unwrap_or(root);
    url.set_path(&format!("{root}/responses/compact"));
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .connect_timeout(Duration::from_secs(15))
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let mut request = client
        .post(url)
        .headers(
            crate::native_engine::request_options::RequestOptions {
                session_id: session.map(str::to_owned),
                compact: true,
            }
            .headers(provider)?,
        )
        .header("x-request-id", uuid::Uuid::new_v4().to_string())
        .json(&body);
    if !provider.provider.api_key.is_empty() {
        request = request.bearer_auth(&provider.provider.api_key);
    }
    let mut response = request
        .send()
        .await
        .context("Native compaction gateway request failed")?;
    ensure!(response.status().is_success(), "Native compaction gateway returned HTTP {}; verify /responses/compact support and input context size", response.status().as_u16());
    const LIMIT: usize = 32 * 1024 * 1024;
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            bytes.len() + chunk.len() <= LIMIT,
            "Native compaction response exceeds 32 MiB"
        );
        bytes.extend_from_slice(&chunk);
    }
    let result: Value = serde_json::from_slice(&bytes).context("Invalid native compaction JSON")?;
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
    Ok(GeneratedSummary {
        text: "Native Responses compaction completed. The encrypted context is saved and will be restored for this provider/model. Earlier messages remain in session history.".into(),
        native: Some(json!({"api":"openai-responses","compaction":true,"items":items,"usage":result["usage"],
            "provider":provider.provider.id,"base_url":provider.provider.base_url.trim_end_matches('/'),"model":provider.model.id})),
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
                store.db.with_conn(|conn| conn.execute_batch("CREATE TRIGGER fail_timestamp BEFORE UPDATE OF updated_at ON conversations BEGIN SELECT RAISE(ABORT, 'failure'); END;")).unwrap().unwrap();
            }
            let before = store.history(&id).unwrap();
            let summary = GeneratedSummary {
                text: "Native context".into(),
                native: Some(json!({"items":[{"type":"compaction","encrypted_content":"opaque"}]})),
            };
            assert!(commit_with_context(&store.db, prepared, &summary, None).is_err());
            assert_eq!(store.history(&id).unwrap(), before);
            let count = store.db.with_conn(|conn| conn.query_row("SELECT COUNT(*) FROM peripheral_settings WHERE key LIKE 'compaction.native.%'", [], |r| r.get::<_, i64>(0))).unwrap().unwrap();
            assert_eq!(count, 0);
        }
    }
}
