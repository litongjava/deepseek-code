//! Discovery of the model IDs an endpoint advertises.
use crate::config::ModelConfig;
use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde_json::Value;
use std::collections::HashSet;
use std::time::Duration;

const LIST_TIMEOUT: Duration = Duration::from_secs(15);
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

fn model_id(entry: &Value) -> Option<String> {
    let raw = entry
        .get("id")
        .or_else(|| entry.get("name"))
        .and_then(Value::as_str)?;
    let id = raw.strip_prefix("models/").unwrap_or(raw).trim();
    (!id.is_empty()).then(|| id.to_owned())
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

fn page_url(url: &str, pairs: &[(&str, &str)]) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(url).with_context(|| format!("Could not parse {url}"))?;
    if !pairs.is_empty() {
        let mut query = url.query_pairs_mut();
        for (name, value) in pairs {
            query.append_pair(name, value);
        }
    }
    Ok(url)
}

async fn get_page(client: &Client, url: reqwest::Url, auth: Auth, api_key: &str) -> Result<Value> {
    let response = authenticated(client.get(url.clone()), auth, api_key)
        .send()
        .await
        .with_context(|| format!("Could not reach {url}"))?;
    let status = response.status();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = response
        .text()
        .await
        .context("Could not read the response")?;
    if is_web_page(&body, content_type.as_deref()) {
        return Err(web_page_error(status));
    }
    if !status.is_success() {
        bail!("The endpoint answered {status}: {}", excerpt(&body));
    }
    serde_json::from_str(&body)
        .with_context(|| format!("The response is not JSON: {}", excerpt(&body)))
}

fn model_entries<'a>(value: &'a Value, field: &str) -> Result<&'a [Value]> {
    value
        .get(field)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .with_context(|| {
            format!(
                "The response has no model list: {}",
                excerpt(&value.to_string())
            )
        })
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
            Some(cursor) => page_url(url, &[("limit", PAGE_SIZE), ("after_id", cursor)])?,
            None => page_url(url, &[("limit", PAGE_SIZE)])?,
        };
        let value = get_page(client, page, Auth::Anthropic, api_key).await?;
        models.extend(model_entries(&value, "data")?.iter().filter_map(model_id));
        if !value
            .get("has_more")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return finish(models);
        }
        let next = value
            .get("last_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .context("The endpoint reported more models but returned no pagination cursor")?;
        if !seen.insert(next.to_owned()) {
            bail!("The endpoint repeated the pagination cursor {next}; the model list is incomplete");
        }
        cursor = Some(next.to_owned());
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
            Some(token) => page_url(url, &[("pageSize", PAGE_SIZE), ("pageToken", token)])?,
            None => page_url(url, &[("pageSize", PAGE_SIZE)])?,
        };
        let value = get_page(client, page, Auth::Gemini, api_key).await?;
        models.extend(model_entries(&value, "models")?.iter().filter_map(model_id));
        let next = value
            .get("nextPageToken")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or_default();
        if next.is_empty() {
            return finish(models);
        }
        if !seen.insert(next.to_owned()) {
            bail!("The endpoint repeated the pagination token {next}; the model list is incomplete");
        }
        token = Some(next.to_owned());
    }
    bail!("The model list did not finish after {MAX_PAGES} pages")
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
            let value = get_page(&client, page_url(&url, &[])?, auth, api_key).await?;
            let entries = model_entries(&value, "data").or_else(|_| model_entries(&value, "models"))?;
            finish(entries.iter().filter_map(model_id).collect())
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
        let id = id.trim();
        if !id.is_empty() && seen.insert(id) {
            ordered.push(id);
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
    use axum::{http::HeaderMap, routing::get, Json, Router};

    async fn serve(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        format!("http://{address}")
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
                Json(serde_json::json!({"data": [{"id": "claude-fixture"}]}))
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
        let error = list_models("anthropic", &base, "fixture-key").await.unwrap_err();
        assert!(error.to_string().contains("repeated the pagination cursor"), "{error:#}");
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
        let error = list_models("gemini", &base, "fixture-key").await.unwrap_err();
        assert!(error.to_string().contains("repeated the pagination token"), "{error:#}");
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
