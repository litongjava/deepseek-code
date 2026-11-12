//! Discovery of the model IDs an endpoint advertises.
use crate::config::ModelConfig;
use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde_json::Value;
use std::collections::HashSet;
use std::time::Duration;

/// Backstop for a caller that passes no deadline of its own. Callers that apply
/// their own total timeout (20 seconds in the CLI) stay authoritative, so their
/// message is the one a stalled endpoint reports.
const LIST_TIMEOUT: Duration = Duration::from_secs(30);
/// A page cap keeps a misbehaving cursor from looping forever.
const MAX_PAGES: usize = 50;
const PAGE_SIZE: &str = "1000";

#[derive(Clone, Copy)]
enum Auth {
    Bearer,
    Anthropic,
    Gemini,
}

/// The listing URL and auth style for each protocol. OpenAI-compatible servers
/// answer `/v1/models`; Anthropic keeps `/v1` in some proxy addresses; Gemini
/// uses `/v1beta/models`.
fn endpoint(protocol: &str, base_url: &str) -> (String, Auth) {
    let base = base_url.trim().trim_end_matches('/');
    match protocol.to_ascii_lowercase().as_str() {
        "anthropic" | "claude" => {
            let url = if base.contains("/v1") {
                format!("{base}/models")
            } else {
                format!("{base}/v1/models")
            };
            (url, Auth::Anthropic)
        }
        "gemini" => {
            let url = if base.contains("/v1beta") {
                format!("{base}/models")
            } else {
                format!("{base}/v1beta/models")
            };
            (url, Auth::Gemini)
        }
        _ => {
            // normalize_base_url strips a trailing /v1 or /chat/completions.
            let base =
                crate::native_engine::provider_manager::ProviderManager::normalize_base_url(base.strip_suffix("/responses").unwrap_or(base));
            (format!("{base}/v1/models"), Auth::Bearer)
        }
    }
}

/// A readable excerpt for error messages; response bodies are untrusted data.
fn excerpt(body: &str) -> String {
    let body = body.trim();
    if body.is_empty() {
        return "empty response".into();
    }
    body.chars().take(200).collect()
}

fn starts_with_ignore_case(value: &str, prefix: &str) -> bool {
    value
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

/// Web pages mean the address is not an API base; their HTML is not useful.
fn is_web_page(body: &str, content_type: Option<&str>) -> bool {
    let head = body.trim_start();
    starts_with_ignore_case(head, "<!doctype html")
        || starts_with_ignore_case(head, "<html")
        || content_type.is_some_and(|value| value.to_ascii_lowercase().contains("text/html"))
}

fn web_page_error(status: reqwest::StatusCode) -> anyhow::Error {
    anyhow::anyhow!(
        "The endpoint answered {status} with a web page, not an API response; check that the Base URL is the API base (it usually ends in /v1)."
    )
}

/// What one list entry's model ID means for the protocol that returned it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum IdField {
    /// `id` (or the `name` some compatible servers return), kept verbatim.
    Exact,
    /// Gemini resource names carry a `models/` prefix that is not part of the ID.
    GeminiResource,
}

fn model_id(entry: &Value, field: IdField, index: usize) -> Result<String> {
    let raw = match field {
        IdField::GeminiResource => entry.get("name"),
        IdField::Exact => entry.get("id").or_else(|| entry.get("name")),
    };
    let raw = match raw {
        Some(Value::String(raw)) => raw.as_str(),
        Some(_) => bail!(
            "Model list entry {} has a model ID that is not a string; the model list is incomplete",
            index + 1
        ),
        None => bail!(
            "Model list entry {} has no model ID; the model list is incomplete",
            index + 1
        ),
    };
    let id = match field {
        IdField::GeminiResource => raw.strip_prefix("models/").unwrap_or(raw),
        IdField::Exact => raw,
    };
    // The CLI stores `provider::model`, so whitespace, control characters and
    // `::` cannot round-trip; rewriting or dropping an ID would delete models.
    if id.is_empty()
        || id.chars().any(|c| c.is_whitespace() || c.is_control())
        || id.contains("::")
    {
        bail!(
            "Model list entry {} has an unusable model ID; the model list is incomplete",
            index + 1
        );
    }
    Ok(id.to_owned())
}

fn model_ids(entries: &[Value], field: IdField) -> Result<Vec<String>> {
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| model_id(entry, field, index))
        .collect()
}

fn authenticated(
    request: reqwest::RequestBuilder,
    auth: Auth,
    api_key: &str,
) -> reqwest::RequestBuilder {
    match auth {
        Auth::Bearer => request.bearer_auth(api_key),
        Auth::Anthropic => request
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01"),
        Auth::Gemini => request.header("x-goog-api-key", api_key),
    }
}

/// Percent-encodings of a key, allowing each character either literal or
/// escaped, because a path segment and a query value escape different sets.
/// Returns nothing when the combinations would grow too large.
fn percent_variants(key: &str) -> Vec<String> {
    const MAX_VARIANTS: usize = 512;
    let mut variants = vec![String::new()];
    for byte in key.bytes() {
        let mut choices = Vec::new();
        if byte.is_ascii_alphanumeric() {
            choices.push((byte as char).to_string());
        } else {
            choices.push(format!("%{byte:02X}"));
            choices.push(format!("%{byte:02x}"));
            let legal_in_a_path = byte.is_ascii_graphic()
                && !matches!(
                    byte,
                    b'%' | b'&' | b'=' | b'?' | b'#' | b'"' | b'\'' | b'<' | b'>' | b'`' | b' '
                );
            if legal_in_a_path {
                choices.push((byte as char).to_string());
            }
        }
        if variants.len().saturating_mul(choices.len()) > MAX_VARIANTS {
            return Vec::new();
        }
        let mut next = Vec::with_capacity(variants.len() * choices.len());
        for prefix in &variants {
            for choice in &choices {
                next.push(format!("{prefix}{choice}"));
            }
        }
        variants = next;
    }
    variants
}

/// Every textual form a credential can take in a response body or a URL.
/// Endpoints quote keys after encoding them (a query string, a resource path,
/// a JSON blob), so redacting only the literal key is not enough.
fn secret_forms(key: &str) -> Vec<String> {
    use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
    use base64::Engine as _;

    let mut forms = vec![key.to_owned()];
    forms.extend(percent_variants(key));
    let unreserved = |byte: u8| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
    };
    // The two uniform conventions, in case the combinations were too many.
    for (encode_reserved_only, upper) in [(false, true), (false, false), (true, true), (true, false)]
    {
        let mut encoded = String::with_capacity(key.len());
        for byte in key.bytes() {
            let literal = if encode_reserved_only {
                unreserved(byte)
            } else {
                byte.is_ascii_alphanumeric()
            };
            if literal {
                encoded.push(byte as char);
            } else if upper {
                encoded.push_str(&format!("%{byte:02X}"));
            } else {
                encoded.push_str(&format!("%{byte:02x}"));
            }
        }
        forms.push(encoded);
    }
    for engine in [&STANDARD, &STANDARD_NO_PAD, &URL_SAFE, &URL_SAFE_NO_PAD] {
        forms.push(engine.encode(key));
    }
    forms.sort_by_key(|form| std::cmp::Reverse(form.len()));
    forms.dedup();
    forms
}

/// Removes every encoding of a credential from text shown to the user.
pub fn redact_secrets(text: &str, api_key: &str) -> String {
    let key = api_key.trim();
    if key.is_empty() {
        return text.to_owned();
    }
    let mut redacted = text.to_owned();
    for form in secret_forms(key) {
        if !form.is_empty() {
            redacted = redacted.replace(&form, "[redacted]");
        }
    }
    redacted
}

/// The endpoint address without credentials or query values, for messages.
/// A cursor in the query is not a credential, but it is a token from the
/// endpoint and is not repeated either. Credentials embedded in the path are
/// replaced in every encoding.
pub fn display_url(raw: &str, api_key: &str) -> String {
    let shown = if let Ok(url) = reqwest::Url::parse(raw) {
        match url.host_str() {
            Some(host) => {
                let port = url.port().map(|port| format!(":{port}")).unwrap_or_default();
                format!("{}://{host}{port}{}", url.scheme(), url.path())
            }
            None => url.path().to_owned(),
        }
    } else {
        // Not a URL: drop anything that could be user information or a query.
        let rest = raw.rsplit('@').next().unwrap_or(raw);
        rest.split(['?', '#']).next().unwrap_or(rest).to_owned()
    };
    redact_secrets(&shown, api_key)
}

fn page_url(url: &str, pairs: &[(&str, &str)], api_key: &str) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(url)
        .with_context(|| format!("Could not parse {}", display_url(url, api_key)))?;
    if !pairs.is_empty() {
        let mut query = url.query_pairs_mut();
        for (name, value) in pairs {
            query.append_pair(name, value);
        }
    }
    Ok(url)
}

async fn get_page(client: &Client, url: reqwest::Url, auth: Auth, api_key: &str) -> Result<Value> {
    let response = match authenticated(client.get(url.clone()), auth, api_key)
        .send()
        .await
    {
        Ok(response) => response,
        // The transport error repeats the full URL, including a cursor in the
        // query, so only a classified reason is kept.
        Err(error) => {
            let reason = if error.is_timeout() {
                "the request timed out"
            } else if error.is_connect() {
                "the connection failed"
            } else {
                "the request failed"
            };
            bail!("Could not reach {}: {reason}", display_url(url.as_str(), api_key));
        }
    };
    let status = response.status();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = match response.text().await {
        Ok(body) => body,
        Err(_) => bail!(
            "Could not read the response from {}",
            display_url(url.as_str(), api_key)
        ),
    };
    if is_web_page(&body, content_type.as_deref()) {
        return Err(web_page_error(status));
    }
    if !status.is_success() {
        bail!("The endpoint answered {status}: {}", redacted_excerpt(&body, api_key));
    }
    serde_json::from_str(&body)
        .with_context(|| format!("The response is not JSON: {}", redacted_excerpt(&body, api_key)))
}

/// An excerpt that never repeats the credential, in any encoding, if the
/// endpoint echoes it. Only a bounded window is scanned so a large body does
/// not multiply by the number of encodings.
fn redacted_excerpt(body: &str, api_key: &str) -> String {
    let window: String = body.chars().take(4096).collect();
    excerpt(&redact_secrets(&window, api_key))
}

fn model_entries<'a>(value: &'a Value, fields: &[&str]) -> Result<&'a [Value]> {
    for field in fields {
        if let Some(entries) = value.get(*field) {
            // Never echo the body: a 200 response can repeat the credential.
            return match entries {
                Value::Array(entries) => Ok(entries.as_slice()),
                _ => bail!(
                    "The response field {field} is not a model list; the model list is incomplete"
                ),
            };
        }
    }
    match fields {
        [field] => bail!("The response has no {field} model list; the model list is incomplete"),
        _ => bail!(
            "The response has no model list in {}; the model list is incomplete",
            fields.join(" or ")
        ),
    }
}

fn finish(mut models: Vec<String>) -> Result<Vec<String>> {
    models.sort();
    models.dedup();
    if models.is_empty() {
        bail!("The endpoint returned no models");
    }
    Ok(models)
}

/// Anthropic pages with `after_id` and reports `has_more` plus `last_id`.
async fn list_anthropic(client: &Client, url: &str, api_key: &str) -> Result<Vec<String>> {
    let mut models = Vec::new();
    let mut seen = HashSet::new();
    let mut cursor: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let page = match &cursor {
            Some(cursor) => page_url(url, &[("limit", PAGE_SIZE), ("after_id", cursor)], api_key)?,
            None => page_url(url, &[("limit", PAGE_SIZE)], api_key)?,
        };
        let value = get_page(client, page, Auth::Anthropic, api_key).await?;
        models.extend(model_ids(model_entries(&value, &["data"])?, IdField::Exact)?);
        // `has_more` is the only completeness signal: a missing or non-boolean
        // value cannot be treated as the last page.
        match value.get("has_more") {
            Some(Value::Bool(false)) => return finish(models),
            Some(Value::Bool(true)) => {}
            Some(_) => bail!(
                "The endpoint returned a non-boolean has_more field; the model list is incomplete"
            ),
            None => bail!(
                "The endpoint did not report whether more models follow; the model list is incomplete"
            ),
        }
        let next = match value.get("last_id") {
            Some(Value::String(id)) if !id.trim().is_empty() => id.trim().to_owned(),
            Some(Value::String(_)) | None => bail!(
                "The endpoint reported more models but returned no pagination cursor; the model list is incomplete"
            ),
            Some(_) => bail!(
                "The endpoint returned a pagination cursor that is not a string; the model list is incomplete"
            ),
        };
        if !seen.insert(next.clone()) {
            // The cursor is an endpoint token; do not print it.
            bail!("The endpoint repeated a pagination cursor; the model list is incomplete");
        }
        cursor = Some(next);
    }
    bail!("The model list did not finish after {MAX_PAGES} pages")
}

/// Gemini pages with `pageToken` and reports the next one as `nextPageToken`.
async fn list_gemini(client: &Client, url: &str, api_key: &str) -> Result<Vec<String>> {
    let mut models = Vec::new();
    let mut seen = HashSet::new();
    let mut token: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let page = match &token {
            Some(token) => page_url(url, &[("pageSize", PAGE_SIZE), ("pageToken", token)], api_key)?,
            None => page_url(url, &[("pageSize", PAGE_SIZE)], api_key)?,
        };
        let value = get_page(client, page, Auth::Gemini, api_key).await?;
        models.extend(model_ids(
            model_entries(&value, &["models"])?,
            IdField::GeminiResource,
        )?);
        // An absent token is the last page; any other type is a malformed
        // response, not an ending.
        let next = match value.get("nextPageToken") {
            None | Some(Value::Null) => return finish(models),
            Some(Value::String(token)) if token.trim().is_empty() => return finish(models),
            Some(Value::String(token)) => token.trim().to_owned(),
            Some(_) => bail!(
                "The endpoint returned a non-string nextPageToken; the model list is incomplete"
            ),
        };
        if !seen.insert(next.clone()) {
            // The token is an endpoint value; do not print it.
            bail!("The endpoint repeated a pagination token; the model list is incomplete");
        }
        token = Some(next);
    }
    bail!("The model list did not finish after {MAX_PAGES} pages")
}

/// OpenAI-compatible list endpoints normally return every model. If one says
/// otherwise, its cursor is unknown, so the list is incomplete and must not be
/// used for a destructive sync.
fn ensure_single_page(value: &Value) -> Result<()> {
    match value.get("has_more") {
        None | Some(Value::Bool(false)) => {}
        Some(Value::Bool(true)) => bail!(
            "The endpoint reported more models than one page; its pagination is not supported, so the model list would be incomplete"
        ),
        Some(_) => bail!(
            "The endpoint returned a non-boolean has_more field; the model list would be incomplete"
        ),
    }
    for field in ["next", "next_page_token", "nextPageToken"] {
        match value.get(field) {
            None | Some(Value::Null) => {}
            Some(Value::String(cursor)) if cursor.trim().is_empty() => {}
            Some(Value::String(_)) => bail!(
                "The endpoint reported more models than one page; its pagination is not supported, so the model list would be incomplete"
            ),
            Some(_) => bail!(
                "The endpoint returned a {field} pagination cursor that is not a string; the model list would be incomplete"
            ),
        }
    }
    Ok(())
}

/// Fetches the model IDs a provider advertises, sorted and de-duplicated.
/// Every page is read before anything is returned, so a partial list is never
/// mistaken for the complete one.
///
/// `protocol` is the same value saved in `provider_type`: `openai`,
/// `anthropic` or `gemini`.
pub async fn list_models(protocol: &str, base_url: &str, api_key: &str) -> Result<Vec<String>> {
    let (url, auth) = endpoint(protocol, base_url);
    let client = Client::builder()
        .timeout(LIST_TIMEOUT)
        .build()
        .context("Could not create the HTTP client")?;
    match auth {
        Auth::Bearer => {
            let value = get_page(&client, page_url(&url, &[], api_key)?, auth, api_key).await?;
            ensure_single_page(&value)?;
            let entries = model_entries(&value, &["data", "models"])?;
            finish(model_ids(entries, IdField::Exact)?)
        }
        Auth::Anthropic => list_anthropic(&client, &url, api_key).await,
        Auth::Gemini => list_gemini(&client, &url, api_key).await,
    }
}

/// The effect of replacing a saved model list with the IDs the endpoint
/// advertises.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ModelSync {
    pub added: Vec<String>,
    pub removed: Vec<String>,
    pub kept: Vec<String>,
}

/// Keeps every saved model the endpoint still advertises, with its local
/// settings, and appends new IDs in upstream order. Duplicate upstream IDs are
/// collapsed. `new_model` supplies the template for an ID the provider did not
/// have before; new models start enabled.
pub fn apply_upstream(
    existing: &[ModelConfig],
    upstream: &[String],
    new_model: impl Fn(&str) -> ModelConfig,
) -> (Vec<ModelConfig>, ModelSync) {
    let mut ordered: Vec<&str> = Vec::new();
    let mut seen = HashSet::new();
    for id in upstream {
        // IDs arrive validated; never trim or rewrite them into new IDs.
        if !id.is_empty() && seen.insert(id.as_str()) {
            ordered.push(id.as_str());
        }
    }
    let wanted: HashSet<&str> = ordered.iter().copied().collect();
    let mut models = Vec::with_capacity(ordered.len());
    let mut local = HashSet::new();
    let mut sync = ModelSync::default();
    for model in existing {
        if !local.insert(model.id.as_str()) {
            continue;
        }
        if wanted.contains(model.id.as_str()) {
            sync.kept.push(model.id.clone());
            models.push(model.clone());
        } else {
            sync.removed.push(model.id.clone());
        }
    }
    for id in ordered {
        if local.contains(id) {
            continue;
        }
        let mut model = new_model(id);
        model.id = id.to_owned();
        model.enabled = true;
        sync.added.push(id.to_owned());
        models.push(model);
    }
    (models, sync)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{http::HeaderMap, response::IntoResponse, routing::get, Json, Router};

    async fn serve(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{address}")
    }

    fn model_route(protocol: &str) -> &'static str {
        if protocol == "gemini" {
            "/v1beta/models"
        } else {
            "/v1/models"
        }
    }

    /// Serves one body for the protocol's list path and returns the base URL.
    async fn serve_listing(protocol: &str, body: Value) -> String {
        let route = model_route(protocol);
        let router = Router::new().route(
            route,
            get(move || {
                let body = body.clone();
                async move { Json(body) }
            }),
        );
        serve(router).await
    }

    async fn query_error(protocol: &str, body: Value) -> String {
        let route = model_route(protocol);
        let router = Router::new().route(
            route,
            get(move || {
                let body = body.clone();
                async move { Json(body) }
            }),
        );
        let base = serve(router).await;
        format!(
            "{:#}",
            list_models(protocol, &base, "fixture-key").await.unwrap_err()
        )
    }

    #[tokio::test]
    async fn openai_listing_uses_bearer_auth_and_removes_duplicates() {
        let router = Router::new().route(
            "/v1/models",
            get(|headers: HeaderMap| async move {
                assert_eq!(headers["authorization"], "Bearer fixture-key");
                Json(serde_json::json!({
                    "object": "list",
                    "data": [{"id": "model-b"}, {"id": "model-a"}, {"id": "model-b"}],
                }))
            }),
        );
        let base = serve(router).await;
        let models = list_models("openai", &format!("{base}/v1"), "fixture-key")
            .await
            .unwrap();
        assert_eq!(models, ["model-a", "model-b"]);
        let responses = list_models("responses", &format!("{base}/v1/responses"), "fixture-key").await.unwrap();
        assert_eq!(responses, models);
    }

    #[tokio::test]
    async fn anthropic_listing_sends_the_api_key_and_version() {
        let router = Router::new().route(
            "/v1/models",
            get(|headers: HeaderMap| async move {
                assert_eq!(headers["x-api-key"], "fixture-key");
                assert_eq!(headers["anthropic-version"], "2023-06-01");
                Json(serde_json::json!({"data": [{"id": "claude-fixture"}], "has_more": false}))
            }),
        );
        let base = serve(router).await;
        let models = list_models("anthropic", &base, "fixture-key")
            .await
            .unwrap();
        assert_eq!(models, ["claude-fixture"]);
    }

    #[tokio::test]
    async fn gemini_listing_strips_the_models_prefix() {
        let router = Router::new().route(
            "/v1beta/models",
            get(|headers: HeaderMap| async move {
                assert_eq!(headers["x-goog-api-key"], "fixture-key");
                Json(serde_json::json!({"models": [{"name": "models/gemini-fixture"}]}))
            }),
        );
        let base = serve(router).await;
        let models = list_models("gemini", &base, "fixture-key").await.unwrap();
        assert_eq!(models, ["gemini-fixture"]);
    }

    #[tokio::test]
    async fn listing_failures_explain_the_status() {
        let router = Router::new().route(
            "/v1/models",
            get(|| async {
                (
                    axum::http::StatusCode::UNAUTHORIZED,
                    Json(serde_json::json!({"error": "invalid key"})),
                )
            }),
        );
        let base = serve(router).await;
        let error = list_models("openai", &base, "bad-key").await.unwrap_err();
        assert!(error.to_string().contains("401"), "{error:#}");
    }

    #[tokio::test]
    async fn web_page_answers_are_reported_without_their_html() {
        let router = Router::new().route(
            "/v1/models",
            get(|| async {
                (
                    axum::http::StatusCode::NOT_FOUND,
                    [(
                        axum::http::header::CONTENT_TYPE,
                        "text/html; charset=utf-8",
                    )],
                    "<!DOCTYPE html><html lang=\"en\"><body>not found</body></html>",
                )
            }),
        );
        let base = serve(router).await;
        let error = list_models("openai", &base, "fixture-key").await.unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("web page"), "{message}");
        assert!(!message.contains("<html"), "{message}");
        assert!(!message.contains("DOCTYPE"), "{message}");
    }

    #[test]
    fn html_detection_accepts_content_type_or_body() {
        assert!(is_web_page("{}", Some("text/html; charset=utf-8")));
        assert!(is_web_page("  <!doctype HTML><html>", None));
        assert!(is_web_page("<html><body></body></html>", None));
        assert!(!is_web_page(r#"{"data":[]}"#, Some("application/json")));
    }

    use axum::extract::Query;
    use std::collections::HashMap;

    #[tokio::test]
    async fn anthropic_listing_reads_every_page() {
        let router = Router::new().route(
            "/v1/models",
            get(|Query(query): Query<HashMap<String, String>>| async move {
                assert_eq!(query.get("limit").map(String::as_str), Some("1000"));
                match query.get("after_id").map(String::as_str) {
                    None => Json(serde_json::json!({
                        "data": [{"id": "claude-a"}, {"id": "claude-b"}],
                        "has_more": true,
                        "last_id": "claude-b",
                    })),
                    Some("claude-b") => Json(serde_json::json!({
                        "data": [{"id": "claude-c"}],
                        "has_more": false,
                        "last_id": "claude-c",
                    })),
                    Some(other) => panic!("unexpected cursor {other}"),
                }
            }),
        );
        let base = serve(router).await;
        let models = list_models("anthropic", &base, "fixture-key").await.unwrap();
        assert_eq!(models, ["claude-a", "claude-b", "claude-c"]);
    }

    #[tokio::test]
    async fn anthropic_listing_rejects_a_cursor_that_repeats() {
        let router = Router::new().route(
            "/v1/models",
            get(|| async {
                Json(serde_json::json!({
                    "data": [{"id": "claude-a"}],
                    "has_more": true,
                    "last_id": "same-cursor",
                }))
            }),
        );
        let base = serve(router).await;
        let error = format!(
            "{:#}",
            list_models("anthropic", &base, "fixture-key").await.unwrap_err()
        );
        assert!(error.contains("repeated a pagination cursor"), "{error}");
        assert!(!error.contains("same-cursor"), "{error}");
    }

    #[tokio::test]
    async fn anthropic_listing_rejects_a_missing_cursor() {
        let router = Router::new().route(
            "/v1/models",
            get(|| async {
                Json(serde_json::json!({"data": [{"id": "claude-a"}], "has_more": true}))
            }),
        );
        let base = serve(router).await;
        let error = list_models("anthropic", &base, "fixture-key").await.unwrap_err();
        assert!(error.to_string().contains("no pagination cursor"), "{error:#}");
    }

    #[tokio::test]
    async fn gemini_listing_reads_every_page() {
        let router = Router::new().route(
            "/v1beta/models",
            get(|Query(query): Query<HashMap<String, String>>| async move {
                assert_eq!(query.get("pageSize").map(String::as_str), Some("1000"));
                match query.get("pageToken").map(String::as_str) {
                    None => Json(serde_json::json!({
                        "models": [{"name": "models/gemini-a"}],
                        "nextPageToken": "page-2",
                    })),
                    Some("page-2") => Json(serde_json::json!({
                        "models": [{"name": "models/gemini-b"}],
                    })),
                    Some(other) => panic!("unexpected token {other}"),
                }
            }),
        );
        let base = serve(router).await;
        let models = list_models("gemini", &base, "fixture-key").await.unwrap();
        assert_eq!(models, ["gemini-a", "gemini-b"]);
    }

    #[tokio::test]
    async fn gemini_listing_rejects_a_repeated_token() {
        let router = Router::new().route(
            "/v1beta/models",
            get(|| async {
                Json(serde_json::json!({
                    "models": [{"name": "models/gemini-a"}],
                    "nextPageToken": "same-token",
                }))
            }),
        );
        let base = serve(router).await;
        let error = format!(
            "{:#}",
            list_models("gemini", &base, "fixture-key").await.unwrap_err()
        );
        assert!(error.contains("repeated a pagination token"), "{error}");
        assert!(!error.contains("same-token"), "{error}");
    }

    #[tokio::test]
    async fn an_empty_list_is_an_error() {
        let router = Router::new().route("/v1/models", get(|| async { Json(serde_json::json!({"data": []})) }));
        let base = serve(router).await;
        let error = list_models("openai", &base, "fixture-key").await.unwrap_err();
        assert!(error.to_string().contains("no models"), "{error:#}");
    }

    #[tokio::test]
    async fn a_non_json_body_is_reported_without_the_whole_payload() {
        let router = Router::new().route(
            "/v1/models",
            get(|| async {
                (
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    "this is not json",
                )
            }),
        );
        let base = serve(router).await;
        let error = list_models("openai", &base, "fixture-key").await.unwrap_err();
        assert!(format!("{error:#}").contains("not JSON"), "{error:#}");
    }

    #[tokio::test]
    async fn a_hanging_endpoint_is_aborted_by_the_query_timeout() {
        // The CLI applies one total timeout to the whole query, as here.
        let router = Router::new().route(
            "/v1/models",
            get(|| async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Json(serde_json::json!({"data": [{"id": "late"}]}))
            }),
        );
        let base = serve(router).await;
        let result = tokio::time::timeout(
            Duration::from_millis(200),
            list_models("openai", &base, "fixture-key"),
        )
        .await;
        assert!(result.is_err(), "the query must be cancellable by its timeout");
    }

    #[tokio::test]
    async fn a_failed_page_aborts_the_whole_query() {
        let router = Router::new().route(
            "/v1/models",
            get(|Query(query): Query<HashMap<String, String>>| async move {
                match query.get("after_id").map(String::as_str) {
                    None => (
                        axum::http::StatusCode::OK,
                        Json(serde_json::json!({
                            "data": [{"id": "claude-a"}],
                            "has_more": true,
                            "last_id": "page-2",
                        })),
                    )
                        .into_response(),
                    _ => (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({"error": "page failed"})),
                    )
                        .into_response(),
                }
            }),
        );
        let base = serve(router).await;
        let error = list_models("anthropic", &base, "fixture-key").await.unwrap_err();
        assert!(error.to_string().contains("500"), "{error:#}");
    }

    #[tokio::test]
    async fn an_openai_page_that_reports_more_is_rejected() {
        let router = Router::new().route(
            "/v1/models",
            get(|| async {
                Json(serde_json::json!({
                    "data": [{"id": "model-a"}],
                    "has_more": true,
                }))
            }),
        );
        let base = serve(router).await;
        let error = list_models("openai", &base, "fixture-key").await.unwrap_err();
        assert!(error.to_string().contains("pagination is not supported"), "{error:#}");
    }

    #[tokio::test]
    async fn an_unusable_entry_aborts_the_whole_list() {
        for entry in [
            serde_json::json!({"id": 42}),
            serde_json::json!({"id": ""}),
            serde_json::json!({"id": "with space"}),
            serde_json::json!({"id": "provider::model"}),
            serde_json::json!({"display_name": "no id at all"}),
        ] {
            let body = serde_json::json!({"data": [{"id": "keep"}, entry]});
            let error = query_error("openai", body).await;
            assert!(error.contains("model list is incomplete"), "{error}");
        }
    }

    #[tokio::test]
    async fn anthropic_requires_a_boolean_has_more() {
        for body in [
            serde_json::json!({"data": [{"id": "claude-a"}], "has_more": "true"}),
            serde_json::json!({"data": [{"id": "claude-a"}]}),
        ] {
            let error = query_error("anthropic", body).await;
            assert!(error.contains("incomplete"), "{error}");
        }
    }

    #[tokio::test]
    async fn gemini_rejects_a_non_string_page_token() {
        let error = query_error(
            "gemini",
            serde_json::json!({"models": [{"name": "models/gemini-a"}], "nextPageToken": 42}),
        )
        .await;
        assert!(error.contains("nextPageToken"), "{error}");
    }

    #[tokio::test]
    async fn openai_rejects_malformed_pagination_metadata() {
        for body in [
            serde_json::json!({"data": [{"id": "model-a"}], "has_more": "yes"}),
            serde_json::json!({"data": [{"id": "model-a"}], "next": 42}),
        ] {
            let error = query_error("openai", body).await;
            assert!(error.contains("incomplete"), "{error}");
        }
    }

    #[tokio::test]
    async fn a_missing_list_error_does_not_repeat_the_api_key() {
        let error = query_error(
            "openai",
            serde_json::json!({"error": "invalid key fixture-key"}),
        )
        .await;
        assert!(!error.contains("fixture-key"), "{error}");
        assert!(error.contains("no model list"), "{error}");
    }

    #[tokio::test]
    async fn model_ids_keep_the_protocol_meaning() {
        assert_eq!(
            list_models("openai", &serve_listing("openai", serde_json::json!({"data": [{"id": "models/keep"}]})).await, "fixture-key")
                .await
                .unwrap(),
            ["models/keep"],
            "OpenAI and Responses IDs are saved verbatim"
        );
        assert_eq!(
            list_models("anthropic", &serve_listing("anthropic", serde_json::json!({"data": [{"id": "models/keep"}], "has_more": false})).await, "fixture-key")
                .await
                .unwrap(),
            ["models/keep"],
            "Anthropic IDs are saved verbatim"
        );
        assert_eq!(
            list_models("gemini", &serve_listing("gemini", serde_json::json!({"models": [{"name": "models/gemini-a"}]})).await, "fixture-key")
                .await
                .unwrap(),
            ["gemini-a"],
            "only a Gemini resource name loses its models/ prefix"
        );
    }

    #[tokio::test]
    async fn a_repeated_cursor_is_not_repeated_in_the_error() {
        let error = query_error(
            "anthropic",
            serde_json::json!({
                "data": [{"id": "claude-a"}],
                "has_more": true,
                "last_id": "secret-cursor-token",
            }),
        )
        .await;
        assert!(error.contains("repeated"), "{error}");
        assert!(!error.contains("secret-cursor-token"), "{error}");
    }

    #[test]
    fn display_url_hides_userinfo_and_query_values() {
        assert_eq!(
            display_url(
                "https://user:pass@example.com/v1/models?after_id=cursor",
                "fixture-key"
            ),
            "https://example.com/v1/models"
        );
        assert_eq!(
            display_url("not a url?token=secret", "fixture-key"),
            "not a url"
        );
    }

    #[test]
    fn display_url_redacts_a_key_in_the_path_in_every_encoding() {
        let key = "fixture-key/with+special";
        let path_forms = [
            key.to_owned(),
            urlencoding::encode(key).into_owned(),
            key.replace('/', "%2F"),
        ];
        for form in path_forms {
            let shown = display_url(&format!("https://example.com/{form}/v1/models"), key);
            assert!(!shown.contains(&form), "leaked {form} in {shown}");
            assert!(shown.contains("[redacted]"), "{shown}");
        }
    }

    #[tokio::test]
    async fn an_encoded_key_in_an_error_body_is_redacted() {
        use base64::engine::general_purpose::STANDARD;
        use base64::Engine as _;
        let key = "fixture-secret-key";
        let encoded = [
            urlencoding::encode(key).into_owned(),
            key.replace('-', "%2D"),
            STANDARD.encode(key),
        ];
        let body = format!(
            "rejected credential: {key} {}",
            encoded.join(" ")
        );
        let router = Router::new().route(
            "/v1/models",
            get(move || {
                let body = body.clone();
                async move { (axum::http::StatusCode::UNAUTHORIZED, body) }
            }),
        );
        let base = serve(router).await;
        let error = format!(
            "{:#}",
            list_models("openai", &base, key).await.unwrap_err()
        );
        assert!(!error.contains(key), "{error}");
        for form in encoded {
            assert!(!error.contains(&form), "leaked {form} in {error}");
        }
        assert!(error.contains("redacted"), "{error}");
    }

    #[tokio::test]
    async fn a_key_in_the_endpoint_path_is_redacted_in_an_unreachable_error() {
        let key = "fixture-path-key";
        let error = format!(
            "{:#}",
            list_models(
                "openai",
                &format!("http://127.0.0.1:1/{key}/v1?token=cursor"),
                key
            )
            .await
            .unwrap_err()
        );
        assert!(error.contains("127.0.0.1:1"), "{error}");
        assert!(!error.contains(key), "{error}");
        assert!(!error.contains("cursor"), "{error}");
        assert!(error.contains("[redacted]"), "{error}");
    }

    #[tokio::test]
    async fn an_unreachable_endpoint_does_not_print_the_url_query() {
        let error = format!(
            "{:#}",
            list_models("openai", "http://127.0.0.1:1/v1?token=secret", "fixture-key")
                .await
                .unwrap_err()
        );
        assert!(error.contains("127.0.0.1:1"), "{error}");
        assert!(!error.contains("secret"), "{error}");
    }

    #[tokio::test]
    async fn a_missing_model_list_is_an_error() {
        let router = Router::new().route(
            "/v1/models",
            get(|| async { Json(serde_json::json!({"object": "list"})) }),
        );
        let base = serve(router).await;
        let error = list_models("openai", &base, "fixture-key").await.unwrap_err();
        assert!(error.to_string().contains("no model list"), "{error:#}");
    }

    #[tokio::test]
    async fn errors_never_repeat_the_api_key() {
        let router = Router::new().route(
            "/v1/models",
            get(|headers: HeaderMap| async move {
                let echoed = headers["authorization"].to_str().unwrap_or_default();
                (
                    axum::http::StatusCode::UNAUTHORIZED,
                    Json(serde_json::json!({"error": format!("invalid credential {echoed}")})),
                )
            }),
        );
        let base = serve(router).await;
        let error = format!(
            "{:#}",
            list_models("openai", &base, "fixture-secret-key").await.unwrap_err()
        );
        assert!(!error.contains("fixture-secret-key"), "{error}");
        assert!(error.contains("redacted"), "{error}");
    }

    fn saved(id: &str, name: &str, context: Option<u32>, enabled: bool) -> ModelConfig {
        ModelConfig {
            id: id.into(),
            name: name.into(),
            enabled,
            max_tokens: Some(4096),
            supports_vision: true,
            supports_text: true,
            api: None,
            supports_tools: true,
            supports_streaming: true,
            context_window: context,
            cost_per_1k_input: None,
            cost_per_1k_output: None,
        }
    }

    fn template(id: &str) -> ModelConfig {
        saved(id, id, None, false)
    }

    #[test]
    fn upstream_sync_keeps_local_settings_and_reports_every_change() {
        let existing = [
            saved("kept", "My name", Some(1234), false),
            saved("gone", "gone", None, true),
        ];
        let upstream = ["kept".to_owned(), "new".to_owned()];
        let (models, sync) = apply_upstream(&existing, &upstream, template);
        assert_eq!(sync.kept, ["kept"]);
        assert_eq!(sync.removed, ["gone"]);
        assert_eq!(sync.added, ["new"]);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].name, "My name");
        assert_eq!(models[0].context_window, Some(1234));
        assert!(!models[0].enabled, "a kept model keeps its local switch");
        assert_eq!(models[1].id, "new");
        assert!(models[1].enabled, "a new model starts enabled");
    }

    #[test]
    fn upstream_sync_dedupes_upstream_and_local_ids() {
        let existing = [saved("a", "a", None, true), saved("a", "duplicate", None, true)];
        let upstream = ["b".to_owned(), "b".to_owned(), "a".to_owned()];
        let (models, sync) = apply_upstream(&existing, &upstream, template);
        assert_eq!(sync.kept, ["a"]);
        assert_eq!(sync.removed, Vec::<String>::new());
        assert_eq!(sync.added, ["b"]);
        assert_eq!(
            models.iter().map(|model| model.id.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
    }
}
