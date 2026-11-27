//! Bounded web tools: page markup must not consume the model's context window.
use anyhow::{anyhow, bail, Result};
use futures::{stream::FuturesUnordered, StreamExt};
use scraper::{ElementRef, Html, Selector};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    io::Read,
    sync::{LazyLock, Mutex},
    time::{Duration, Instant},
};

const MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const DEFAULT_CHARS: usize = 12_000;
const MAX_CHARS: usize = 50_000;
const FETCH_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
const SEARCH_TIMEOUT: Duration = Duration::from_secs(5);
const JINA_TIMEOUT: Duration = Duration::from_secs(30);
/// Reader fallback for pages whose text is hidden behind client-side rendering.
const JINA_READER_BASE: &str = "https://r.jina.ai/";
const MAX_REDIRECTS: usize = 10;
const THIN_HTML_CHARS: u64 = 200;
const THIN_HTML_MIN_BYTES: usize = 1024;
const USER_AGENT: &str = concat!(
    "Mozilla/5.0 (compatible; DeepSeekCode/",
    env!("CARGO_PKG_VERSION"),
    "; WebReader)"
);

fn http_url(url: &str) -> Result<reqwest::Url> {
    let parsed = reqwest::Url::parse(url)?;
    if !matches!(parsed.scheme(), "http" | "https") {
        bail!("Only HTTP(S) URLs are supported");
    }
    Ok(parsed)
}

/// Public HTTP URLs are upgraded to HTTPS. Local HTTP(S) URLs retain their
/// scheme and require a capability issued for the reviewed call.
pub(super) fn model_url(url: &str, local_approved: bool) -> Result<String> {
    let mut parsed = http_url(url)?;
    let local = crate::permissions::web_fetch::requires_local_approval(url);
    if local && !local_approved {
        bail!("WebFetch requires approval to access this local or private address");
    }
    if parsed.scheme() == "http" && !local {
        parsed
            .set_scheme("https")
            .map_err(|_| anyhow!("Cannot upgrade {url} to HTTPS"))?;
    }
    Ok(parsed.to_string())
}

const CACHE_TTL: Duration = Duration::from_secs(15 * 60);
const CACHE_ENTRIES: usize = 32;

/// Final URL, content type, body and whether the body was cut at the limit.
#[derive(Clone)]
struct Page {
    final_url: String,
    content_type: String,
    bytes: Vec<u8>,
    truncated: bool,
    /// Set when the text came from the Jina Reader fallback.
    via: Option<&'static str>,
}

impl Page {
    fn render(&self, input: &Value) -> Result<Value> {
        let mut result = page_result(
            input,
            &self.final_url,
            &self.content_type,
            &self.bytes,
            self.truncated,
        )?;
        if let Some(via) = self.via {
            result["via"] = Value::String(via.to_owned());
        }
        Ok(result)
    }
}

async fn render_page(page: Page, input: Value) -> Result<Value> {
    // Parsing a large document is CPU work; keep it off the async executor.
    tokio::task::spawn_blocking(move || page.render(&input)).await?
}

/// Downloads are reused for 15 minutes per URL, as in the reference CLI.
static CACHE: LazyLock<Mutex<HashMap<String, (Instant, Page)>>> = LazyLock::new(Default::default);

fn cached(url: &str) -> Option<Page> {
    let cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    cache
        .get(url)
        .filter(|(at, _)| at.elapsed() < CACHE_TTL)
        .map(|(_, page)| page.clone())
}

fn remember(url: &str, page: &Page) {
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    cache.retain(|_, (at, _)| at.elapsed() < CACHE_TTL);
    if cache.len() >= CACHE_ENTRIES {
        if let Some(oldest) = cache
            .iter()
            .min_by_key(|(_, (at, _))| *at)
            .map(|(key, _)| key.clone())
        {
            cache.remove(&oldest);
        }
    }
    cache.insert(url.to_owned(), (Instant::now(), page.clone()));
}

/// Redirects up to ten hops are followed, including cross-host ones. A local
/// or private target is refused unless the original URL was itself local, so a
/// public page cannot bounce the request onto the internal network.
fn redirect_policy(allow_local_target: bool) -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() > MAX_REDIRECTS {
            return attempt.error("too many redirects");
        }
        if !matches!(attempt.url().scheme(), "http" | "https") {
            return attempt.error("Redirect to unsupported URL scheme");
        }
        if !allow_local_target
            && crate::permissions::web_fetch::requires_local_approval(attempt.url().as_str())
        {
            return attempt.error(
                "WebFetch refuses to follow a redirect from a public address to a local or private address",
            );
        }
        attempt.follow()
    })
}

fn selector(value: &str) -> Selector {
    Selector::parse(value).expect("static selector")
}

fn render_element(element: ElementRef<'_>, base: &reqwest::Url, text: &mut String, depth: usize) {
    if depth > 128 {
        return;
    }
    let tag = element.value().name();
    if matches!(
        tag,
        "script"
            | "style"
            | "svg"
            | "noscript"
            | "template"
            | "nav"
            | "footer"
            | "aside"
            | "form"
            | "button"
            | "iframe"
    ) || element.value().attr("hidden").is_some()
        || element.value().attr("aria-hidden") == Some("true")
        || matches!(
            element.value().attr("role"),
            Some("navigation" | "banner" | "contentinfo")
        )
    {
        return;
    }
    let block = matches!(
        tag,
        "p" | "div"
            | "section"
            | "article"
            | "main"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "li"
            | "tr"
            | "pre"
            | "blockquote"
            | "br"
    );
    if block {
        text.push('\n');
    }
    for child in element.children() {
        if let Some(value) = child.value().as_text() {
            text.push_str(value);
        } else if let Some(child) = ElementRef::wrap(child) {
            render_element(child, base, text, depth + 1);
        }
    }
    if tag == "a" {
        if let Some(href) = element
            .value()
            .attr("href")
            .filter(|href| !href.starts_with('#'))
        {
            if let Ok(url) = base.join(href) {
                if matches!(url.scheme(), "http" | "https") {
                    text.push_str(" (");
                    text.push_str(url.as_str());
                    text.push(')');
                }
            }
        }
    }
    if matches!(tag, "td" | "th") {
        text.push_str(" | ");
    }
    if block {
        text.push('\n');
    }
}

fn readable_html(html: &str, url: &reqwest::Url) -> (String, String) {
    let document = Html::parse_document(html);
    let title = document
        .select(&selector("title"))
        .next()
        .map(|title| title.text().collect::<String>())
        .unwrap_or_default();
    let main = document
        .select(&selector("main, [role=main]"))
        .next()
        .or_else(|| document.select(&selector("article")).next())
        .or_else(|| document.select(&selector("body")).next())
        .unwrap_or_else(|| document.root_element());
    let mut text = String::new();
    render_element(main, url, &mut text, 0);
    let text = text
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    (title, text)
}

fn page_result(
    input: &Value,
    url: &str,
    content_type: &str,
    bytes: &[u8],
    source_truncated: bool,
) -> Result<Value> {
    let content_type_lower = content_type.to_ascii_lowercase();
    let is_text = content_type_lower.starts_with("text/")
        || ["json", "xml", "javascript"]
            .iter()
            .any(|kind| content_type_lower.contains(kind));
    if !is_text {
        bail!("Unsupported content type {content_type}; WebFetch reads text and HTML pages");
    }
    let body = String::from_utf8_lossy(bytes);
    let raw = input["raw"].as_bool().unwrap_or(false);
    let is_html = content_type_lower.contains("html")
        || body.trim_start().starts_with("<!DOCTYPE html")
        || body.trim_start().starts_with("<html");
    let (title, text) = if is_html && !raw {
        readable_html(&body, &http_url(url)?)
    } else {
        (String::new(), body.into_owned())
    };
    let (text, collapsed) = super::base64_collapse::collapse(&text);
    let offset = input["offset"].as_u64().unwrap_or(0) as usize;
    let limit = input["max_chars"]
        .as_u64()
        .unwrap_or(DEFAULT_CHARS as u64)
        .clamp(1, MAX_CHARS as u64) as usize;
    let total_chars = text.chars().count();
    let mut content: String = text.chars().skip(offset).take(limit).collect();
    let returned_chars = content.chars().count();
    let has_more = offset.saturating_add(returned_chars) < total_chars;
    if collapsed > 0 && !content.is_empty() {
        content.push('\n');
        content.push_str(&super::base64_collapse::note(collapsed));
    }
    Ok(json!({
        "url":url, "title":title, "content_type":content_type,
        "format":if raw { "raw" } else if is_html { "readable_text" } else { "text" },
        "content":content, "downloaded_bytes":bytes.len(), "total_extracted_chars":total_chars,
        "returned_chars":returned_chars, "offset":offset, "max_chars":limit,
        "truncated":source_truncated || has_more || offset > 0, "source_truncated":source_truncated,
        "next_offset":if has_more { Some(offset + returned_chars) } else { None },
        "note":if source_truncated { Some("The response exceeded the 2 MiB download limit; totals describe only the fetched prefix. Use a more specific page or endpoint.") }
            else if has_more { Some("Additional extracted text is available by fetching this URL with next_offset as offset.") }
            else if total_chars == 0 { Some("No readable text was found. The page may require JavaScript or a different content endpoint.") } else { None }
    }))
}

async fn read_bounded(mut response: reqwest::Response) -> Result<(Vec<u8>, bool)> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        let remaining = MAX_RESPONSE_BYTES.saturating_sub(bytes.len());
        bytes.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        if chunk.len() > remaining {
            return Ok((bytes, true));
        }
    }
    Ok((bytes, false))
}

fn is_cacheable(input: &Value) -> bool {
    input["headers"]
        .as_object()
        .map_or(true, |headers| headers.is_empty())
}

/// Reader fallback is for public pages only, and never for raw reads, a URL
/// already pointing at the reader itself, or a request carrying custom headers
/// that may hold credentials: those must not be forwarded to a third party.
fn jina_fallback_allowed(input: &Value, url: &reqwest::Url) -> bool {
    is_cacheable(input)
        && !input["raw"].as_bool().unwrap_or(false)
        && !crate::permissions::web_fetch::requires_local_approval(url.as_str())
        && url.host_str().map_or(true, |host| {
            !host.trim_end_matches('.').eq_ignore_ascii_case("r.jina.ai")
        })
}

/// Statuses where a second reader attempt is worth the extra request. A 404
/// stays a normal error rather than being retried through another service.
/// Only transport/HTTP failures from the direct request qualify; other errors
/// (unsupported content types, policy refusals) are not retried elsewhere.
fn should_fallback_on_error(error: &anyhow::Error) -> bool {
    match error
        .chain()
        .find_map(|cause| cause.downcast_ref::<reqwest::Error>())
    {
        // Redirect refusals and loops are policy decisions, not transport
        // failures; retrying through the reader would bypass the guard.
        Some(error) if error.is_redirect() => false,
        Some(error) => match error.status() {
            Some(status) => {
                matches!(status.as_u16(), 401 | 403 | 406 | 429) || status.is_server_error()
            }
            None => true,
        },
        None => false,
    }
}

/// Reqwest's `Display` omits the redirect-policy reason, so keep the source
/// chain in the message the model sees.
fn error_chain(error: &anyhow::Error) -> String {
    let mut message = error.to_string();
    let mut current: Option<&(dyn std::error::Error + 'static)> = Some(error.as_ref());
    while let Some(cause) = current {
        let text = cause.to_string();
        if !text.is_empty() && !message.contains(&text) {
            message.push_str(": ");
            message.push_str(&text);
        }
        current = cause.source();
    }
    message
}

/// A large HTML download that yields almost no readable text is usually a
/// client-side rendered shell; the reader may still return its content.
fn is_thin_html(page: &Page, result: &Value, input: &Value) -> bool {
    !input["raw"].as_bool().unwrap_or(false)
        && page.content_type.to_ascii_lowercase().contains("html")
        && result["format"] == "readable_text"
        && page.bytes.len() >= THIN_HTML_MIN_BYTES
        && result["total_extracted_chars"].as_u64().unwrap_or(0) < THIN_HTML_CHARS
}

fn async_client(proxy: Option<&str>, allow_local_target: bool) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .connect_timeout(FETCH_CONNECT_TIMEOUT)
        .user_agent(USER_AGENT)
        .redirect(redirect_policy(allow_local_target));
    if allow_local_target {
        // A host proxy variable must not hijack a loopback or private request,
        // and a local URL must never be sent to an external proxy either.
        builder = builder.no_proxy();
    } else if let Some(proxy) = proxy.map(str::trim).filter(|value| !value.is_empty()) {
        builder = builder.proxy(reqwest::Proxy::all(proxy)?);
    }
    Ok(builder.build()?)
}

fn blocking_client(
    proxy: Option<&str>,
    allow_local_target: bool,
) -> Result<reqwest::blocking::Client> {
    let mut builder = reqwest::blocking::Client::builder()
        .timeout(FETCH_TIMEOUT)
        .connect_timeout(FETCH_CONNECT_TIMEOUT)
        .user_agent(USER_AGENT)
        .redirect(redirect_policy(allow_local_target));
    if allow_local_target {
        builder = builder.no_proxy();
    } else if let Some(proxy) = proxy.map(str::trim).filter(|value| !value.is_empty()) {
        builder = builder.proxy(reqwest::Proxy::all(proxy)?);
    }
    Ok(builder.build()?)
}

async fn direct_page_async(
    client: &reqwest::Client,
    url: &reqwest::Url,
    input: &Value,
) -> Result<Page> {
    let mut request = client.get(url.clone());
    if let Some(headers) = input["headers"].as_object() {
        for (key, value) in headers {
            if let Some(value) = value.as_str() {
                request = request.header(key, value);
            }
        }
    }
    let response = request.send().await?.error_for_status()?;
    let final_url = response.url().to_string();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("text/plain")
        .to_string();
    let (bytes, truncated) = read_bounded(response).await?;
    Ok(Page {
        final_url,
        content_type,
        bytes,
        truncated,
        via: None,
    })
}

async fn jina_page_async(
    client: &reqwest::Client,
    url: &reqwest::Url,
    jina_base: &str,
) -> Result<Page> {
    let response = client
        .get(format!("{jina_base}{}", url.as_str()))
        .header(reqwest::header::ACCEPT, "text/markdown")
        .timeout(JINA_TIMEOUT)
        .send()
        .await?
        .error_for_status()?;
    // Keep the reader's real media type: an HTML error or challenge page must
    // still go through readable_html instead of being returned as raw markup.
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .unwrap_or_else(|| "text/markdown".to_owned());
    let (bytes, truncated) = read_bounded(response).await?;
    if bytes.is_empty() {
        bail!("Jina Reader returned an empty response");
    }
    Ok(Page {
        final_url: url.to_string(),
        content_type,
        bytes,
        truncated,
        via: Some("jina_reader"),
    })
}

/// `fetch`, plus a proxy fixed by configuration and a reader endpoint that
/// tests can point at a local mock.
pub(crate) async fn fetch_with_options(
    input: Value,
    proxy: Option<&str>,
    jina_base: &str,
) -> Result<Value> {
    let url = http_url(
        input["url"]
            .as_str()
            .ok_or_else(|| anyhow!("url required"))?,
    )?;
    let cacheable = is_cacheable(&input);
    if cacheable {
        if let Some(page) = cached(url.as_str()) {
            return render_page(page, input).await;
        }
    }
    let allow_local_target = crate::permissions::web_fetch::requires_local_approval(url.as_str());
    let client = async_client(proxy, allow_local_target)?;

    let direct_page = match direct_page_async(&client, &url, &input).await {
        Ok(page) => page,
        Err(error) => {
            if jina_fallback_allowed(&input, &url) && should_fallback_on_error(&error) {
                match jina_page_async(&client, &url, jina_base).await {
                    Ok(page) => {
                        let result = render_page(page.clone(), input).await?;
                        if cacheable {
                            remember(url.as_str(), &page);
                        }
                        return Ok(result);
                    }
                    Err(jina_error) => {
                        return Err(anyhow!(
                            "WebFetch failed: {}; Jina Reader fallback failed: {}",
                            error_chain(&error),
                            error_chain(&jina_error)
                        ))
                    }
                }
            }
            return Err(anyhow!(error_chain(&error)));
        }
    };

    // A rendering failure is not a transport failure: unsupported binary
    // content such as images or archives must not go to a third-party reader.
    let result = render_page(direct_page.clone(), input.clone())
        .await
        .map_err(|error| anyhow!(error_chain(&error)))?;

    if jina_fallback_allowed(&input, &url) && is_thin_html(&direct_page, &result, &input) {
        if let Ok(page) = jina_page_async(&client, &url, jina_base).await {
            if let Ok(reader_result) = render_page(page.clone(), input).await {
                if cacheable {
                    remember(url.as_str(), &page);
                }
                return Ok(reader_result);
            }
        }
    }
    if cacheable {
        remember(url.as_str(), &direct_page);
    }
    Ok(result)
}

pub(crate) async fn fetch_with_proxy(input: Value, proxy: Option<&str>) -> Result<Value> {
    fetch_with_options(input, proxy, JINA_READER_BASE).await
}

pub(super) async fn fetch(input: Value) -> Result<Value> {
    fetch_with_proxy(input, None).await
}

pub(crate) fn fetch_blocking_with_options(
    input: Value,
    proxy: Option<&str>,
    jina_base: &str,
) -> Result<Value> {
    let url = http_url(
        input["url"]
            .as_str()
            .ok_or_else(|| anyhow!("url required"))?,
    )?;
    let cacheable = is_cacheable(&input);
    if cacheable {
        if let Some(page) = cached(url.as_str()) {
            return page.render(&input);
        }
    }
    let allow_local_target = crate::permissions::web_fetch::requires_local_approval(url.as_str());
    let client = blocking_client(proxy, allow_local_target)?;

    let direct_page = match direct_page_blocking(&client, &url, &input) {
        Ok(page) => page,
        Err(error) => {
            if jina_fallback_allowed(&input, &url) && should_fallback_on_error(&error) {
                match jina_page_blocking(&client, &url, jina_base) {
                    Ok(page) => {
                        let result = page.render(&input)?;
                        if cacheable {
                            remember(url.as_str(), &page);
                        }
                        return Ok(result);
                    }
                    Err(jina_error) => {
                        return Err(anyhow!(
                            "WebFetch failed: {}; Jina Reader fallback failed: {}",
                            error_chain(&error),
                            error_chain(&jina_error)
                        ))
                    }
                }
            }
            return Err(anyhow!(error_chain(&error)));
        }
    };

    let result = direct_page
        .render(&input)
        .map_err(|error| anyhow!(error_chain(&error)))?;

    if jina_fallback_allowed(&input, &url) && is_thin_html(&direct_page, &result, &input) {
        if let Ok(page) = jina_page_blocking(&client, &url, jina_base) {
            if let Ok(reader_result) = page.render(&input) {
                if cacheable {
                    remember(url.as_str(), &page);
                }
                return Ok(reader_result);
            }
        }
    }
    if cacheable {
        remember(url.as_str(), &direct_page);
    }
    Ok(result)
}

pub(crate) fn fetch_blocking_with_proxy(input: Value, proxy: Option<&str>) -> Result<Value> {
    fetch_blocking_with_options(input, proxy, JINA_READER_BASE)
}

pub(super) fn fetch_blocking(input: Value) -> Result<Value> {
    fetch_blocking_with_proxy(input, None)
}

fn direct_page_blocking(
    client: &reqwest::blocking::Client,
    url: &reqwest::Url,
    input: &Value,
) -> Result<Page> {
    let mut request = client.get(url.clone());
    if let Some(headers) = input["headers"].as_object() {
        for (key, value) in headers {
            if let Some(value) = value.as_str() {
                request = request.header(key, value);
            }
        }
    }
    let response = request.send()?.error_for_status()?;
    let final_url = response.url().to_string();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("text/plain")
        .to_string();
    let mut bytes = Vec::new();
    response
        .take(MAX_RESPONSE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    let truncated = bytes.len() > MAX_RESPONSE_BYTES;
    bytes.truncate(MAX_RESPONSE_BYTES);
    Ok(Page {
        final_url,
        content_type,
        bytes,
        truncated,
        via: None,
    })
}

fn jina_page_blocking(
    client: &reqwest::blocking::Client,
    url: &reqwest::Url,
    jina_base: &str,
) -> Result<Page> {
    let response = client
        .get(format!("{jina_base}{}", url.as_str()))
        .header(reqwest::header::ACCEPT, "text/markdown")
        .timeout(JINA_TIMEOUT)
        .send()?
        .error_for_status()?;
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .unwrap_or_else(|| "text/markdown".to_owned());
    let mut bytes = Vec::new();
    response
        .take(MAX_RESPONSE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    let truncated = bytes.len() > MAX_RESPONSE_BYTES;
    bytes.truncate(MAX_RESPONSE_BYTES);
    if bytes.is_empty() {
        bail!("Jina Reader returned an empty response");
    }
    Ok(Page {
        final_url: url.to_string(),
        content_type,
        bytes,
        truncated,
        via: Some("jina_reader"),
    })
}

fn clipped(value: &str, limit: usize) -> String {
    value.chars().take(limit).collect()
}

fn plain(value: &str) -> String {
    Html::parse_fragment(
        value
            .trim()
            .trim_start_matches("<![CDATA[")
            .trim_end_matches("]]>"),
    )
    .root_element()
    .text()
    .collect::<Vec<_>>()
    .join(" ")
    .split_whitespace()
    .collect::<Vec<_>>()
    .join(" ")
}

fn parse_search(source: &str, body: &str) -> Vec<Value> {
    let mut results = Vec::new();
    let mut urls = HashSet::new();
    let mut add = |title: String, url: String, snippet: String| {
        if let Ok(url) = http_url(&url) {
            if !title.is_empty() && urls.insert(url.to_string()) && results.len() < 8 {
                results.push(json!({"title":clipped(&title, 200), "url":url.as_str(), "snippet":clipped(&snippet, 700)}));
            }
        }
    };
    if source == "duckduckgo_html" {
        let document = Html::parse_document(body);
        for result in document.select(&selector(".result")) {
            if let Some(link) = result.select(&selector("a.result__a")).next() {
                let href = link.value().attr("href").unwrap_or("");
                let url = reqwest::Url::parse("https://duckduckgo.com")
                    .unwrap()
                    .join(href)
                    .ok()
                    .map(|url| {
                        url.query_pairs()
                            .find(|(key, _)| key == "uddg")
                            .map(|(_, value)| value.into_owned())
                            .unwrap_or_else(|| url.to_string())
                    })
                    .unwrap_or_default();
                let snippet = result
                    .select(&selector(".result__snippet"))
                    .next()
                    .map(|el| el.text().collect::<String>())
                    .unwrap_or_default();
                add(link.text().collect(), url, snippet);
            }
        }
    } else {
        let items = regex::Regex::new(r"(?is)<item\b[^>]*>(.*?)</item>").unwrap();
        let tag = |item: &str, name: &str| -> String {
            regex::Regex::new(&format!(r"(?is)<{name}\b[^>]*>(.*?)</{name}>"))
                .unwrap()
                .captures(item)
                .and_then(|capture| capture.get(1))
                .map(|value| plain(value.as_str()))
                .unwrap_or_default()
        };
        for item in items.captures_iter(body) {
            add(
                tag(&item[1], "title"),
                tag(&item[1], "link"),
                tag(&item[1], "description"),
            );
        }
    }
    results
}

async fn search_provider(
    client: reqwest::Client,
    source: &'static str,
    url: String,
) -> Result<(&'static str, Vec<Value>)> {
    let response = client.get(url).send().await?.error_for_status()?;
    let (bytes, truncated) = read_bounded(response).await?;
    if truncated {
        bail!("Search response exceeded the download limit");
    }
    let results = parse_search(source, &String::from_utf8_lossy(&bytes));
    if results.is_empty() {
        bail!(
            "No usable results returned (the provider may be unavailable or showing a challenge)"
        );
    }
    Ok((source, results))
}

async fn search_urls(query: &str, endpoints: Vec<(&'static str, String)>) -> Result<Value> {
    let client = reqwest::Client::builder()
        .timeout(SEARCH_TIMEOUT)
        .connect_timeout(Duration::from_secs(3))
        .user_agent(USER_AGENT)
        .build()?;
    let mut pending = FuturesUnordered::new();
    for (source, url) in endpoints {
        let client = client.clone();
        pending.push(async move { (source, search_provider(client, source, url).await) });
    }
    let mut failures = Vec::new();
    while let Some((source, result)) = pending.next().await {
        match result {
            Ok((source, results)) => {
                return Ok(json!({"query":query,"source":source,"results":results}))
            }
            Err(error) => failures.push(format!("{source}: {}", error.without_url_if_reqwest())),
        }
    }
    bail!("Web search unavailable: {}", failures.join("; "))
}

pub(super) async fn search(input: Value) -> Result<Value> {
    let query = input["query"]
        .as_str()
        .filter(|q| !q.trim().is_empty())
        .ok_or_else(|| anyhow!("A nonempty query is required"))?;
    if query.chars().count() > 2_000 {
        bail!("Search queries must be at most 2000 characters");
    }
    let encoded = urlencoding::encode(query);
    search_urls(
        query,
        vec![
            (
                "duckduckgo_html",
                format!("https://html.duckduckgo.com/html/?q={encoded}"),
            ),
            (
                "bing_rss",
                format!("https://www.bing.com/search?q={encoded}&format=rss"),
            ),
        ],
    )
    .await
}

trait SafeError {
    fn without_url_if_reqwest(&self) -> String;
}
impl SafeError for anyhow::Error {
    fn without_url_if_reqwest(&self) -> String {
        if let Some(error) = self.downcast_ref::<reqwest::Error>() {
            if let Some(status) = error.status() {
                format!("HTTP {status}")
            } else if error.is_timeout() {
                "provider timed out".into()
            } else {
                "provider request failed".into()
            }
        } else {
            self.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fetch_timeouts_allow_cold_proxies_and_redirects() {
        assert_eq!(FETCH_CONNECT_TIMEOUT, Duration::from_secs(10));
        assert_eq!(FETCH_TIMEOUT, Duration::from_secs(30));
        assert_eq!(JINA_TIMEOUT, Duration::from_secs(30));
        assert!(FETCH_CONNECT_TIMEOUT < FETCH_TIMEOUT);
        assert!(JINA_TIMEOUT >= FETCH_TIMEOUT);
    }

    #[test]
    fn user_agent_names_the_product_and_its_version() {
        assert!(USER_AGENT.contains("DeepSeekCode/"), "{USER_AGENT}");
        assert!(
            USER_AGENT.contains(env!("CARGO_PKG_VERSION")),
            "{USER_AGENT}"
        );
        assert!(
            !USER_AGENT.to_ascii_lowercase().contains("claude"),
            "{USER_AGENT}"
        );
    }

    #[test]
    fn readable_page_omits_markup_and_keeps_main_links() {
        let html = format!("<html><head><title>Release notes</title><script>{}</script></head><body><nav>Navigation</nav><main><header><h1>Release 4</h1></header><p>更快的回复 &amp; tools.</p><a href='/docs'>Read docs</a><script>secret()</script><style>css</style><div hidden>hidden</div></main><footer>Footer</footer></body></html>", "x".repeat(800_000));
        let result = page_result(
            &json!({}),
            "https://example.org/changelog",
            "text/html",
            html.as_bytes(),
            false,
        )
        .unwrap();
        let content = result["content"].as_str().unwrap();
        assert!(content.contains("Release 4"));
        assert!(content.contains("更快的回复 & tools."));
        assert!(content.contains("https://example.org/docs"));
        for removed in ["Navigation", "secret", "css", "hidden", "Footer"] {
            assert!(!content.contains(removed));
        }
        assert!(content.len() < 200);
        assert_eq!(result["format"], "readable_text");
    }

    #[test]
    fn raw_and_readable_results_are_bounded_with_unicode_paging() {
        let body = "中文🙂".repeat(30_000);
        let result = page_result(
            &json!({"offset":2,"max_chars":4}),
            "https://example.org",
            "text/plain",
            body.as_bytes(),
            false,
        )
        .unwrap();
        assert_eq!(result["content"], "🙂中文🙂");
        assert_eq!(result["next_offset"], 6);
        assert_eq!(result["truncated"], true);
        let raw = page_result(
            &json!({"raw":true,"max_chars":999999}),
            "https://example.org",
            "text/html",
            body.as_bytes(),
            true,
        )
        .unwrap();
        assert_eq!(raw["returned_chars"], MAX_CHARS);
        assert_eq!(raw["source_truncated"], true);
        assert_eq!(raw["format"], "raw");
    }

    #[test]
    fn collapses_long_base64_in_page_code() {
        use base64::Engine;
        let json = format!("{{\"request\":\"{}\"}}", "x".repeat(20_000));
        let blob = base64::engine::general_purpose::STANDARD.encode(json.as_bytes());
        let html = format!("<html><head><title>Request</title></head><body><main><p>Example payload:</p><pre>{blob}</pre><p>Done.</p></main></body></html>");
        let placeholder = format!(
            "[base64 data omitted: about {} bytes decoded, looks like JSON]",
            json.len()
        );
        let result = page_result(
            &json!({}),
            "https://example.org/request",
            "text/html",
            html.as_bytes(),
            false,
        )
        .unwrap();
        let content = result["content"].as_str().unwrap();
        assert!(content.contains(&placeholder));
        assert!(content.contains("Example payload:"));
        assert!(content.contains("Done."));
        assert!(content
            .contains("(1 long base64 blocks were collapsed; read with raw: true to see them.)"));
        assert!(!content.contains(&blob[..64]));

        let raw = page_result(
            &json!({"raw":true}),
            "https://example.org/request",
            "text/html",
            html.as_bytes(),
            false,
        )
        .unwrap();
        assert!(raw["content"].as_str().unwrap().contains(&placeholder));
        assert!(!raw["content"].as_str().unwrap().contains(&blob[..64]));
    }

    #[test]
    fn parses_real_search_results_instead_of_instant_answers() {
        let html = "<div class='result'><a class='result__a' href='//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.org%2Frelease'>Release notes</a><div class='result__snippet'>Latest &amp; fastest</div></div>";
        let results = parse_search("duckduckgo_html", html);
        assert_eq!(results[0]["url"], "https://example.org/release");
        assert_eq!(results[0]["snippet"], "Latest & fastest");
        let rss = "<rss><channel><item><title><![CDATA[Release &amp; updates]]></title><link>https://example.org/news?a=1&amp;b=2</link><description>New features</description></item></channel></rss>";
        let results = parse_search("bing_rss", rss);
        assert_eq!(results[0]["title"], "Release & updates");
        assert_eq!(results[0]["url"], "https://example.org/news?a=1&b=2");
        assert!(parse_search("duckduckgo_html", "<html>Verify you are human</html>").is_empty());
    }

    #[tokio::test]
    async fn failed_provider_does_not_delay_valid_provider_or_hide_total_failure() {
        let app = axum::Router::new()
            .route("/fail", axum::routing::get(|| async { axum::http::StatusCode::SERVICE_UNAVAILABLE }))
            .route("/slow", axum::routing::get(|| async { tokio::time::sleep(Duration::from_secs(10)).await; "" }))
            .route("/ok", axum::routing::get(|| async { "<rss><item><title>Release</title><link>https://example.org/news</link><description>Latest</description></item></rss>" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let result = search_urls(
            "news",
            vec![
                ("duckduckgo_html", format!("{base}/fail")),
                ("bing_rss", format!("{base}/ok")),
            ],
        )
        .await
        .unwrap();
        assert_eq!(result["results"][0]["title"], "Release");
        let fast_result = tokio::time::timeout(
            Duration::from_secs(1),
            search_urls(
                "news",
                vec![
                    ("duckduckgo_html", format!("{base}/slow")),
                    ("bing_rss", format!("{base}/ok")),
                ],
            ),
        )
        .await;
        assert!(
            fast_result.is_ok(),
            "A slow provider must not hold up usable search results"
        );
        assert!(
            search_urls("news", vec![("duckduckgo_html", format!("{base}/fail"))])
                .await
                .is_err()
        );
        server.abort();
    }

    #[test]
    fn model_urls_upgrade_public_http_and_require_local_approval() {
        assert_eq!(
            model_url("http://example.com/a?b=1", false).unwrap(),
            "https://example.com/a?b=1"
        );
        assert_eq!(
            model_url("https://docs.rs/", false).unwrap(),
            "https://docs.rs/"
        );
        assert_eq!(
            model_url("http://127.0.0.1:8080/", true).unwrap(),
            "http://127.0.0.1:8080/"
        );
        assert_eq!(
            model_url("http://localhost/", true).unwrap(),
            "http://localhost/"
        );
        for url in [
            "http://localhost:3000/",
            "https://intranet/",
            "http://127.0.0.1/",
            "http://192.168.1.2/",
            "http://[::1]/",
            "http://app.localhost/",
            "ftp://example.com/",
        ] {
            assert!(model_url(url, false).is_err(), "{url}");
        }
    }

    #[tokio::test]
    async fn fetch_caches_pages_and_follows_redirects() {
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = hits.clone();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = axum::Router::new()
            .route(
                "/page",
                axum::routing::get(move || {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    async { "cached body" }
                }),
            )
            .route(
                "/same",
                axum::routing::get(|| async { axum::response::Redirect::temporary("/page") }),
            )
            .route(
                "/away",
                axum::routing::get(move || async move {
                    axum::response::Redirect::temporary(&format!("http://127.0.0.1:{port}/page"))
                }),
            )
            .route(
                "/loop",
                axum::routing::get(|| async { axum::response::Redirect::temporary("/loop") }),
            );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let base = format!("http://127.0.0.1:{port}");
        for _ in 0..2 {
            assert_eq!(
                fetch(json!({"url":format!("{base}/page")})).await.unwrap()["content"],
                "cached body"
            );
        }
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            fetch(json!({"url":format!("{base}/same")})).await.unwrap()["content"],
            "cached body"
        );
        let away = fetch(json!({"url":format!("{base}/away")})).await.unwrap();
        assert_eq!(away["content"], "cached body");
        assert_eq!(away["url"], format!("http://127.0.0.1:{port}/page"));
        assert!(away["redirect_url"].is_null());
        let error = fetch(json!({"url":format!("{base}/loop")}))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("too many redirects"), "{error}");
        server.abort();
    }

    /// A minimal HTTP proxy that answers absolute-form requests, so tests can
    /// exercise public URLs without touching the network.
    type MockResponse = (u16, Vec<(String, String)>, Vec<u8>);

    async fn spawn_mock_proxy<F>(handler: F) -> (String, std::sync::Arc<Mutex<Vec<String>>>)
    where
        F: Fn(&str) -> MockResponse + Send + Sync + 'static,
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let handler = std::sync::Arc::new(handler);
        let seen = std::sync::Arc::new(Mutex::new(Vec::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let recorded = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    break;
                };
                let handler = handler.clone();
                let recorded = recorded.clone();
                tokio::spawn(async move {
                    let mut buffer = Vec::new();
                    let mut chunk = [0u8; 1024];
                    loop {
                        match stream.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                        }
                        if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let request = String::from_utf8_lossy(&buffer);
                    let target = request
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or_default()
                        .to_owned();
                    let accept_markdown = request.lines().any(|line| {
                        line.to_ascii_lowercase()
                            .starts_with("accept: text/markdown")
                    });
                    recorded
                        .lock()
                        .unwrap()
                        .push(format!("{target}\naccept_markdown={accept_markdown}"));
                    let (status, headers, body) = handler(&target);
                    let reason = match status {
                        200 => "OK",
                        302 => "Found",
                        403 => "Forbidden",
                        _ => "Status",
                    };
                    let mut head = format!("HTTP/1.1 {status} {reason}\r\n");
                    for (name, value) in headers {
                        head.push_str(&format!("{name}: {value}\r\n"));
                    }
                    head.push_str(&format!(
                        "Content-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    ));
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(&body).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        (format!("http://{addr}"), seen)
    }

    #[tokio::test]
    async fn explicit_proxy_routes_fetch_requests() {
        let (proxy, seen) = spawn_mock_proxy(|_| {
            (
                200,
                vec![("Content-Type".into(), "text/plain".into())],
                b"proxied body".to_vec(),
            )
        })
        .await;
        let result = fetch_with_proxy(json!({"url":"http://public.example/page"}), Some(&proxy))
            .await
            .unwrap();
        assert_eq!(result["content"], "proxied body");
        assert!(result["via"].is_null());
        let seen = seen.lock().unwrap();
        assert!(
            seen[0].starts_with("http://public.example/page"),
            "the proxy must receive the absolute request URL: {seen:?}"
        );
    }

    #[tokio::test]
    async fn local_targets_bypass_a_configured_proxy() {
        let app = axum::Router::new()
            .route("/page", axum::routing::get(|| async { "local body" }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        // Port 1 has no listener: routing the local request through this proxy
        // would fail instead of returning the page.
        let result = fetch_with_proxy(
            json!({"url": format!("{base}/page")}),
            Some("http://127.0.0.1:1"),
        )
        .await
        .unwrap();
        assert_eq!(result["content"], "local body");
        server.abort();
    }

    #[tokio::test]
    async fn public_cross_host_redirects_are_followed() {
        let (proxy, seen) = spawn_mock_proxy(|target| {
            if target.contains("docs.public.example") {
                (
                    200,
                    vec![("Content-Type".into(), "text/html".into())],
                    b"<html><head><title>Docs</title></head><body><main><p>Cross host docs page.</p></main></body></html>"
                        .to_vec(),
                )
            } else {
                (
                    302,
                    vec![("Location".into(), "http://docs.public.example/page".into())],
                    Vec::new(),
                )
            }
        })
        .await;
        let result = fetch_with_proxy(json!({"url":"http://public.example/start"}), Some(&proxy))
            .await
            .unwrap();
        assert!(result["content"]
            .as_str()
            .unwrap()
            .contains("Cross host docs page."));
        assert_eq!(result["url"], "http://docs.public.example/page");
        let seen = seen.lock().unwrap();
        assert!(
            seen.iter()
                .any(|request| request.starts_with("http://docs.public.example/page")),
            "{seen:?}"
        );
    }

    #[tokio::test]
    async fn public_redirect_to_local_address_is_refused() {
        let leaked = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits = leaked.clone();
        let app = axum::Router::new().route(
            "/secret",
            axum::routing::get(move || {
                hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                async { "internal secret" }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let internal = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let (proxy, _) = spawn_mock_proxy(move |_| {
            (
                302,
                vec![(
                    "Location".into(),
                    format!("http://127.0.0.1:{}/secret", internal.port()),
                )],
                Vec::new(),
            )
        })
        .await;
        let error = fetch_with_proxy(json!({"url":"http://public.example/away"}), Some(&proxy))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("local or private"), "{error}");
        assert_eq!(
            leaked.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "a public redirect must not reach the internal server"
        );
        server.abort();
    }

    #[tokio::test]
    async fn jina_reader_fallback_covers_http_errors_and_thin_html() {
        let shell = format!(
            "<!DOCTYPE html><html><head><title>App</title></head><body><div id='app'></div><script>{}</script></body></html>",
            "x".repeat(2048)
        );
        let (proxy, seen) = spawn_mock_proxy(move |target| {
            if target.contains("jina.test") {
                (
                    200,
                    vec![("Content-Type".into(), "text/markdown".into())],
                    b"# Reader article\n\nReader extracted content.".to_vec(),
                )
            } else if target.contains("/shell") {
                (
                    200,
                    vec![("Content-Type".into(), "text/html".into())],
                    shell.clone().into_bytes(),
                )
            } else {
                (
                    403,
                    vec![("Content-Type".into(), "text/plain".into())],
                    b"Forbidden".to_vec(),
                )
            }
        })
        .await;
        for path in ["article", "shell"] {
            let result = fetch_with_options(
                json!({"url":format!("http://public.example/{path}")}),
                Some(&proxy),
                "http://jina.test/",
            )
            .await
            .unwrap();
            assert_eq!(result["via"], "jina_reader", "{path}");
            assert_eq!(result["url"], format!("http://public.example/{path}"));
            assert!(result["content"]
                .as_str()
                .unwrap()
                .contains("Reader extracted content."));
        }
        let seen = seen.lock().unwrap();
        for path in ["article", "shell"] {
            assert!(
                seen.iter().any(|request| request
                    .starts_with(&format!("http://jina.test/http://public.example/{path}"))
                    && request.contains("accept_markdown=true")),
                "{path}: {seen:?}"
            );
        }
    }

    #[tokio::test]
    async fn custom_headers_disable_the_reader_fallback() {
        let (proxy, seen) = spawn_mock_proxy(|_| {
            (
                403,
                vec![("Content-Type".into(), "text/plain".into())],
                b"Forbidden".to_vec(),
            )
        })
        .await;
        let error = fetch_with_options(
            json!({"url":"http://public.example/private","headers":{"Authorization":"Bearer secret"}}),
            Some(&proxy),
            "http://jina.test/",
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("403"), "{error}");
        let seen = seen.lock().unwrap();
        assert_eq!(
            seen.len(),
            1,
            "a request with custom headers must not be sent to the reader: {seen:?}"
        );
        assert!(
            !seen
                .iter()
                .any(|request| request.starts_with("http://jina.test/")),
            "{seen:?}"
        );
    }

    #[tokio::test]
    async fn binary_pages_do_not_use_the_reader_fallback() {
        let (proxy, seen) = spawn_mock_proxy(|_| {
            (
                200,
                vec![("Content-Type".into(), "image/png".into())],
                vec![0x89, b'P', b'N', b'G'],
            )
        })
        .await;
        let error = fetch_with_options(
            json!({"url":"http://public.example/image.png"}),
            Some(&proxy),
            "http://jina.test/",
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("Unsupported content type"), "{error}");
        let seen = seen.lock().unwrap();
        assert!(
            !seen
                .iter()
                .any(|request| request.starts_with("http://jina.test/")),
            "unsupported binary content must not be retried through the reader: {seen:?}"
        );
    }

    #[tokio::test]
    async fn uppercase_html_content_type_still_triggers_the_reader_fallback() {
        let shell = format!(
            "<!DOCTYPE html><html><head><title>App</title></head><body><div id='app'></div><script>{}</script></body></html>",
            "x".repeat(2048)
        );
        let (proxy, _) = spawn_mock_proxy(move |target| {
            if target.contains("jina.test") {
                (
                    200,
                    vec![("Content-Type".into(), "text/markdown".into())],
                    b"reader content".to_vec(),
                )
            } else {
                (
                    200,
                    vec![("Content-Type".into(), "Text/HTML".into())],
                    shell.clone().into_bytes(),
                )
            }
        })
        .await;
        let result = fetch_with_options(
            json!({"url":"http://public.example/uppercase-shell"}),
            Some(&proxy),
            "http://jina.test/",
        )
        .await
        .unwrap();
        assert_eq!(result["via"], "jina_reader");
        assert_eq!(result["content"], "reader content");
    }

    #[tokio::test]
    async fn reader_html_pages_are_cleaned_instead_of_returned_as_markup() {
        let (proxy, _) = spawn_mock_proxy(|target| {
            if target.contains("jina.test") {
                (
                    200,
                    vec![("Content-Type".into(), "text/html; charset=utf-8".into())],
                    b"<html><head><title>Reader</title></head><body><main><p>Reader HTML content</p></main></body></html>"
                        .to_vec(),
                )
            } else {
                (
                    403,
                    vec![("Content-Type".into(), "text/plain".into())],
                    b"Forbidden".to_vec(),
                )
            }
        })
        .await;
        let result = fetch_with_options(
            json!({"url":"http://public.example/reader-html"}),
            Some(&proxy),
            "http://jina.test/",
        )
        .await
        .unwrap();
        assert_eq!(result["via"], "jina_reader");
        let content = result["content"].as_str().unwrap();
        assert_eq!(content, "Reader HTML content");
        assert!(!content.contains("<p>"), "{content}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blocking_fetch_uses_the_same_reader_fallback() {
        let (proxy, _) = spawn_mock_proxy(|target| {
            if target.contains("jina.test") {
                (
                    200,
                    vec![("Content-Type".into(), "text/markdown".into())],
                    b"blocking reader content".to_vec(),
                )
            } else {
                (
                    500,
                    vec![("Content-Type".into(), "text/plain".into())],
                    b"failure".to_vec(),
                )
            }
        })
        .await;
        let result = tokio::task::spawn_blocking(move || {
            fetch_blocking_with_options(
                json!({"url":"http://public.example/blocking"}),
                Some(&proxy),
                "http://jina.test/",
            )
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result["via"], "jina_reader");
        assert_eq!(result["content"], "blocking reader content");
    }

    #[tokio::test]
    async fn local_failures_do_not_use_the_reader_fallback() {
        let reader_hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let reader_counter = reader_hits.clone();
        let reader = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let reader_base = format!("http://{}/", reader.local_addr().unwrap());
        let reader_server = tokio::spawn(async move {
            axum::serve(
                reader,
                axum::Router::new().fallback(move || {
                    let counter = reader_counter.clone();
                    async move {
                        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        "unexpected reader response"
                    }
                }),
            )
            .await
            .unwrap();
        });
        let app = axum::Router::new().route(
            "/fail",
            axum::routing::get(|| async { axum::http::StatusCode::FORBIDDEN }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let error = fetch_with_options(json!({"url":format!("{base}/fail")}), None, &reader_base)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("403"), "{error}");
        assert_eq!(
            reader_hits.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "local URLs must not be retried through the reader"
        );
        server.abort();
        reader_server.abort();
    }

    #[tokio::test]
    async fn fetch_caps_download_and_rejects_http_errors() {
        let app = axum::Router::new()
            .route(
                "/large",
                axum::routing::get(|| async { "a".repeat(MAX_RESPONSE_BYTES + 100) }),
            )
            .route(
                "/missing",
                axum::routing::get(|| async { axum::http::StatusCode::NOT_FOUND }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let result = fetch(json!({"url":format!("{base}/large")})).await.unwrap();
        assert_eq!(result["downloaded_bytes"], MAX_RESPONSE_BYTES);
        assert_eq!(result["source_truncated"], true);
        assert_eq!(result["returned_chars"], DEFAULT_CHARS);
        assert!(fetch(json!({"url":format!("{base}/missing")}))
            .await
            .is_err());
        server.abort();
    }

    #[tokio::test]
    #[ignore = "Public network smoke test, run explicitly"]
    async fn live_public_search_and_changelog() {
        let started = std::time::Instant::now();
        let results = search(json!({"query":"OpenAI Codex changelog"}))
            .await
            .unwrap();
        println!(
            "Live search: source={}, results={}, elapsed_ms={}",
            results["source"],
            results["results"].as_array().unwrap().len(),
            started.elapsed().as_millis()
        );
        let started = std::time::Instant::now();
        let page = fetch(json!({"url":"https://developers.openai.com/codex/changelog"}))
            .await
            .unwrap();
        println!("Live changelog: downloaded_bytes={}, extracted_chars={}, returned_chars={}, truncated={}, elapsed_ms={}", page["downloaded_bytes"], page["total_extracted_chars"], page["returned_chars"], page["truncated"], started.elapsed().as_millis());
        assert!(page["returned_chars"].as_u64().unwrap() <= DEFAULT_CHARS as u64);
    }
}
