//! Discovery of the model IDs an endpoint advertises.
use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde_json::Value;
use std::time::Duration;

const LIST_TIMEOUT: Duration = Duration::from_secs(15);

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

/// Fetches the model IDs a provider advertises, sorted and de-duplicated.
///
/// `protocol` is the same value saved in `provider_type`: `openai`,
/// `anthropic` or `gemini`.
pub async fn list_models(protocol: &str, base_url: &str, api_key: &str) -> Result<Vec<String>> {
    let (url, auth) = endpoint(protocol, base_url);
    let client = Client::builder()
        .timeout(LIST_TIMEOUT)
        .build()
        .context("Could not create the HTTP client")?;
    let request = client.get(&url);
    let request = match auth {
        Auth::Bearer => request.bearer_auth(api_key),
        Auth::Anthropic => request
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01"),
        Auth::Gemini => request.header("x-goog-api-key", api_key),
    };
    let response = request
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
    let value: Value = serde_json::from_str(&body)
        .with_context(|| format!("The response is not JSON: {}", excerpt(&body)))?;
    let entries = value
        .get("data")
        .or_else(|| value.get("models"))
        .and_then(Value::as_array)
        .with_context(|| format!("The response has no model list: {}", excerpt(&body)))?;
    let mut models: Vec<String> = entries.iter().filter_map(model_id).collect();
    models.sort();
    models.dedup();
    if models.is_empty() {
        bail!("The endpoint returned no models");
    }
    Ok(models)
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
}
