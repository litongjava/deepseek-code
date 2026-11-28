//! Loopback fixtures for WebFetch: one page server and one model server, so
//! Accept negotiation, media-type labeling and offset paging are exercised
//! over real HTTP without external services.
use super::{web_fetch_with_prompt, ResolvedProvider};
use crate::tools::path_scope::approved_auto_call;
use axum::{
    extract::{Path, State},
    http::{header, HeaderMap},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Recorded {
    pages: Arc<Mutex<Vec<Value>>>,
    model: Arc<Mutex<Vec<Value>>>,
}

struct Fixture {
    base: String,
    recorded: Recorded,
    server: tokio::task::JoinHandle<()>,
}

async fn page(
    State(recorded): State<Recorded>,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> ([(header::HeaderName, &'static str); 1], String) {
    recorded.pages.lock().unwrap().push(json!({
        "accept": headers.get(header::ACCEPT).and_then(|value| value.to_str().ok()),
    }));
    let (mime, body) = match name.as_str() {
        // A case- and parameter-bearing media type must still be Markdown.
        "short" => ("Text/Markdown; charset=utf-8", "# 界🙂\n".to_owned()),
        // Above the 20,000-character summary threshold.
        "long" => ("text/markdown", format!("{}tail", "a".repeat(20_001))),
        "html" => (
            "text/html",
            "<main>Visible<script>hidden</script></main>".to_owned(),
        ),
        _ => panic!("unexpected fixture {name}"),
    };
    ([(header::CONTENT_TYPE, mime)], body)
}

async fn chat(State(recorded): State<Recorded>, Json(body): Json<Value>) -> Json<Value> {
    recorded.model.lock().unwrap().push(body);
    Json(json!({
        "id":"local","object":"chat.completion","created":0,"model":"fixture",
        "choices":[{"index":0,"message":{"role":"assistant","content":"model answer"},
            "finish_reason":"stop"}],
        "usage":{"prompt_tokens":10,"completion_tokens":2,"total_tokens":12}
    }))
}

impl Fixture {
    async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let recorded = Recorded::default();
        let app = Router::new()
            .route("/pages/{name}", get(page))
            .route("/v1/chat/completions", post(chat))
            .with_state(recorded.clone());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            base,
            recorded,
            server,
        }
    }

    fn provider(&self, window: u32, max_tokens: u32) -> ResolvedProvider {
        ResolvedProvider {
            provider: serde_json::from_value(json!({
                "id":"fixture","name":"Fixture","base_url":self.base,
                "api_key":"fixture-only","api_format":"openai","models":[],"enabled":true
            }))
            .unwrap(),
            model: serde_json::from_value(json!({
                "id":"fixture-model","name":"Fixture","enabled":true,
                "supports_web_search":false,"context_window":window,"max_tokens":max_tokens
            }))
            .unwrap(),
        }
    }

    /// The prompted delivery path, under a one-call WebFetch grant.
    async fn fetch(
        &self,
        name: &str,
        offset: usize,
        prompt: &str,
        window: u32,
        max_tokens: u32,
    ) -> Value {
        let input = json!({
            "url": format!("{}/pages/{name}", self.base),
            "offset": offset,
            "prompt": prompt,
        });
        let provider = self.provider(window, max_tokens);
        approved_auto_call(
            "WebFetch",
            &input,
            ".",
            true,
            web_fetch_with_prompt(
                input.clone(),
                &provider,
                None,
                Some((input["url"].as_str().unwrap(), true)),
                crate::native_engine::request_options::RequestOptions::for_session(None),
            ),
        )
        .await
        .unwrap()
    }

    /// The transport path, which keeps the page's own `format` and totals.
    async fn transport(&self, name: &str, offset: usize) -> Value {
        let input = json!({
            "url": format!("{}/pages/{name}", self.base),
            "offset": offset,
            "max_chars": 50_000,
        });
        approved_auto_call(
            "WebFetch",
            &input,
            ".",
            true,
            crate::tools::execute_approved_http_fetch_async(
                input.clone(),
                None,
                input["url"].as_str().unwrap(),
                true,
            ),
        )
        .await
        .unwrap()
    }

    fn page_requests(&self) -> Vec<Value> {
        self.recorded.pages.lock().unwrap().clone()
    }

    fn model_requests(&self) -> Vec<Value> {
        self.recorded.model.lock().unwrap().clone()
    }

    async fn stop(self) {
        self.server.abort();
        let _ = self.server.await;
    }
}

#[tokio::test]
async fn direct_fetches_negotiate_markdown() {
    let f = Fixture::start().await;
    let result = f.fetch("short", 0, "read", 128_000, 8_192).await;
    assert_eq!(result["result"], "# 界🙂\n");
    assert_eq!(result["transport"], "plaintext_http");
    assert_eq!(result["warning"], "Fetched over unencrypted HTTP.");
    let pages = f.page_requests();
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0]["accept"], "text/markdown, text/html, */*");
    f.stop().await;
}

#[tokio::test]
async fn markdown_responses_are_labeled_and_paged_by_character() {
    let f = Fixture::start().await;
    let page = f.transport("short", 0).await;
    assert_eq!(page["format"], "markdown");
    assert_eq!(page["content_type"], "Text/Markdown; charset=utf-8");
    assert_eq!(page["content"], "# 界🙂\n");
    assert_eq!(page["total_extracted_chars"], 5);

    let paged = f.fetch("short", 3, "which part?", 128_000, 8_192).await;
    assert_eq!(paged["result"], "🙂\n");
    assert_eq!(paged["summarized"], false);
    assert!(f.model_requests().is_empty());
    f.stop().await;
}

#[tokio::test]
async fn an_offset_past_the_end_reports_the_page_length_without_a_model_request() {
    let f = Fixture::start().await;
    for (offset, expected) in [
        (
            7,
            "Nothing left to read from offset 7: this page's text is 5 characters long.",
        ),
        (
            5,
            "Nothing left to read from offset 5: this page's text is 5 characters long.",
        ),
    ] {
        let result = f.fetch("short", offset, "read", 128_000, 8_192).await;
        assert_eq!(result["result"], expected);
        assert_eq!(result["warning"], "Fetched over unencrypted HTTP.");
        assert_eq!(result["summarized"], false);
    }
    assert!(f.model_requests().is_empty());
    f.stop().await;
}

#[tokio::test]
async fn html_pages_keep_readable_text_instead_of_raw_markup() {
    let f = Fixture::start().await;
    let result = f.fetch("html", 0, "what is visible?", 128_000, 8_192).await;
    assert_eq!(result["result"], "Visible");
    assert!(f.model_requests().is_empty());
    f.stop().await;
}

#[tokio::test]
async fn a_roomy_context_keeps_the_configured_summary_budget() {
    let f = Fixture::start().await;
    let result = f
        .fetch("long", 0, "what is on the page?", 128_000, 8_192)
        .await;
    assert_eq!(result["result"], "model answer");
    assert_eq!(result["warning"], "Fetched over unencrypted HTTP.");
    assert_eq!(result["summarized"], true);
    let requests = f.model_requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["max_tokens"], 8_192);
    f.stop().await;
}

#[tokio::test]
async fn a_tight_context_reserves_only_the_output_room_that_fits() {
    let f = Fixture::start().await;
    let result = f
        .fetch("long", 0, "what is on the page?", 8_192, 8_192)
        .await;
    assert_eq!(result["result"], "model answer");
    assert_eq!(result["warning"], "Fetched over unencrypted HTTP.");
    assert_eq!(result["summarized"], true);
    let requests = f.model_requests();
    assert_eq!(requests.len(), 1);
    let budget = requests[0]["max_tokens"].as_u64().unwrap();
    assert!(
        budget > 0 && budget < 8_192,
        "a tight context must reserve only the remaining room: {budget}"
    );
    f.stop().await;
}
