//! Shared transport policy. Retries end before a response stream is exposed.
mod request_failure;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::time::Duration;

pub mod retry_events;

pub use request_failure::{
    find_failure, parse_failure_summary, sanitize_message, sanitize_remote_text, sanitized_chain,
    AttemptBudget, FailureSummary, RequestFailure, RequestStage,
};
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ErrorClass {
    Authentication,
    RateLimit,
    ContextLimit,
    Transient,
    Permanent,
}

impl ErrorClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Authentication => "authentication",
            Self::RateLimit => "rate_limit",
            Self::ContextLimit => "context_limit",
            Self::Transient => "transient",
            Self::Permanent => "permanent",
        }
    }
}

#[derive(Debug)]
pub struct ApiError {
    pub status: u16,
    pub retry_after: Option<Duration>,
    pub message: String,
}
impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Model API error {}: {}", self.status, self.message)
    }
}
impl std::error::Error for ApiError {}

pub fn classify(error: &anyhow::Error) -> ErrorClass {
    // A structured failure already carries the classification computed while
    // the typed transport error was still available.
    if let Some(failure) = find_failure(error) {
        return failure.class();
    }
    classify_raw(error)
}

pub(crate) fn classify_raw(error: &anyhow::Error) -> ErrorClass {
    if let Some(api) = request_failure::find_in_chain::<ApiError>(error) {
        return match api.status {
            401 | 403 => ErrorClass::Authentication,
            429 => ErrorClass::RateLimit,
            408 | 500..=599 => ErrorClass::Transient,
            400 | 413 if api.message.to_lowercase().contains("context") || api.status == 413 => {
                ErrorClass::ContextLimit
            }
            _ => ErrorClass::Permanent,
        };
    }
    if let Some(http) = request_failure::find_in_chain::<reqwest::Error>(error) {
        if is_transient_transport(http) {
            return ErrorClass::Transient;
        }
    }
    ErrorClass::Permanent
}

/// Only connection and transfer failures before any response content are
/// retried. Authentication, billing, request parameters, redirects, builder
/// errors and unparseable JSON responses stay permanent.
fn is_transient_transport(error: &reqwest::Error) -> bool {
    if error.is_timeout() || error.is_connect() {
        return true;
    }
    if error.is_builder() || error.is_redirect() || error.status().is_some() {
        return false;
    }
    if !error.is_request() {
        return false;
    }
    source_is_recoverable(error)
}

/// Hyper and IO causes that mean the exchange was interrupted, not rejected.
fn source_is_recoverable(error: &reqwest::Error) -> bool {
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        if let Some(hyper) = cause.downcast_ref::<hyper::Error>() {
            // These raise when a pooled connection closes, a peer resets the
            // stream, or the request body write was aborted; a fresh attempt is safe.
            if hyper.is_incomplete_message()
                || hyper.is_canceled()
                || hyper.is_closed()
                || hyper.is_body_write_aborted()
            {
                return true;
            }
        }
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            use std::io::ErrorKind::*;
            if matches!(
                io.kind(),
                ConnectionReset | ConnectionAborted | BrokenPipe | UnexpectedEof | NotConnected
            ) {
                return true;
            }
        }
        source = cause.source();
    }
    false
}

/// A host-facing, sanitized diagnostic for any model-request failure.
pub fn request_diagnostic(error: &anyhow::Error) -> String {
    match find_failure(error) {
        Some(failure) => failure.diagnostic(),
        None => sanitized_chain(error),
    }
}

pub async fn checked_response(mut response: reqwest::Response) -> Result<reqwest::Response> {
    observe_rate_limits(&response);
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .map(|seconds| Duration::from_secs(seconds.min(300)));
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let body = read_bounded(&mut response).await;
    Err(ApiError {
        status,
        retry_after,
        message: safe_remote_error(&body, &content_type),
    }
    .into())
}

/// A remote error body may echo the request, including prompt text. Only a small
/// set of recognizable error fields is kept; anything else is summarized.
pub fn safe_remote_error(body: &[u8], content_type: &str) -> String {
    let text = match std::str::from_utf8(body) {
        Ok(text) => text.trim(),
        Err(_) => {
            return format!(
                "service returned HTTP error with a non-UTF-8 body ({content_type}, {} bytes)",
                body.len()
            )
        }
    };
    if text.is_empty() {
        return "service returned an HTTP error without a body".to_owned();
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        // A plain-text body is free text: it could be the prompt the server
        // rejected, so it is never reproduced. Only a recognized error category
        // is kept, which preserves authentication, billing, rate-limit and
        // parameter meaning without carrying any of the body.
        let plain = content_type.is_empty() || content_type.starts_with("text/plain");
        if plain {
            return plain_text_error(text);
        }
        return format!(
            "service returned an HTTP error with a non-JSON body ({content_type}, {} bytes); content omitted",
            body.len()
        );
    };
    let mut parts: Vec<String> = Vec::new();
    for path in [
        &["error", "message"][..],
        &["error", "type"][..],
        &["error", "code"][..],
        &["message"][..],
        &["detail"][..],
        &["error_description"][..],
        &["code"][..],
    ] {
        let mut current = &value;
        let mut found = true;
        for key in path {
            match current.get(*key) {
                Some(next) => current = next,
                None => {
                    found = false;
                    break;
                }
            }
        }
        if !found {
            continue;
        }
        if let Some(text) = current.as_str() {
            let text = text.trim();
            if !text.is_empty() && !parts.iter().any(|part| part == text) {
                parts.push(text.to_owned());
            }
        }
    }
    if let Some(text) = value.get("error").and_then(|error| error.as_str()) {
        let text = text.trim();
        if !text.is_empty() && !parts.iter().any(|part| part == text) {
            parts.push(text.to_owned());
        }
    }
    if parts.is_empty() {
        return format!(
            "service returned an HTTP error without a recognized error field ({content_type})"
        );
    }
    request_failure::sanitize_remote_text(&parts.join(": "))
}

/// Classify a plain-text error body without reproducing it.
///
/// Values such as `Unauthorized` or `rate limit exceeded` are common and worth
/// keeping as meaning, but the same channel can echo the request. The response
/// therefore reports only a category derived from recognized wording plus the
/// body length; the body itself never reaches a diagnostic.
fn plain_text_error(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let category = if ["unauthorized", "authentication", "forbidden", "api key", "credential"]
        .iter()
        .any(|word| lower.contains(word))
    {
        "authentication failed"
    } else if ["rate limit", "too many requests", "slow down", "overloaded"]
        .iter()
        .any(|word| lower.contains(word))
    {
        "rate limited"
    } else if ["quota", "billing", "insufficient", "payment", "balance"]
        .iter()
        .any(|word| lower.contains(word))
    {
        "billing or quota error"
    } else if ["timeout", "timed out", "unavailable", "temporarily", "retry later"]
        .iter()
        .any(|word| lower.contains(word))
    {
        "temporarily unavailable"
    } else if ["invalid", "bad request", "malformed", "unsupported", "missing"]
        .iter()
        .any(|word| lower.contains(word))
    {
        "invalid request"
    } else {
        return format!(
            "service returned an HTTP error with a plain-text body ({} bytes); content omitted",
            text.len()
        );
    };
    format!(
        "service reported an error: {category} (plain-text body, {} bytes; content omitted)",
        text.len()
    )
}

/// Read only as much of an error body as a diagnostic could ever use.
async fn read_bounded(response: &mut reqwest::Response) -> Vec<u8> {
    const LIMIT: usize = 64 * 1024;
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                let room = LIMIT.saturating_sub(body.len());
                if room == 0 {
                    break;
                }
                body.extend_from_slice(&chunk[..chunk.len().min(room)]);
                if chunk.len() >= room {
                    break;
                }
            }
            Ok(None) | Err(_) => break,
        }
    }
    body
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RateLimitState {
    pub origin: String,
    pub requests_remaining: Option<u64>,
    pub tokens_remaining: Option<u64>,
    pub requests_reset: Option<String>,
    pub observed_at: String,
}
static RATE_LIMITS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::BTreeMap<String, RateLimitState>>,
> = std::sync::LazyLock::new(Default::default);

fn observe_rate_limits(response: &reqwest::Response) {
    let headers = response.headers();
    let number = |names: &[&str]| {
        names
            .iter()
            .find_map(|name| headers.get(*name)?.to_str().ok()?.parse::<u64>().ok())
    };
    let requests_remaining = number(&[
        "anthropic-ratelimit-requests-remaining",
        "x-ratelimit-remaining-requests",
    ]);
    let tokens_remaining = number(&[
        "anthropic-ratelimit-tokens-remaining",
        "x-ratelimit-remaining-tokens",
    ]);
    if requests_remaining.is_none() && tokens_remaining.is_none() {
        return;
    }
    let origin = response.url().origin().ascii_serialization();
    let requests_reset = [
        "anthropic-ratelimit-requests-reset",
        "x-ratelimit-reset-requests",
    ]
    .iter()
    .find_map(|name| {
        headers
            .get(*name)?
            .to_str()
            .ok()
            .map(|v| v.chars().take(80).collect())
    });
    if let Ok(mut states) = RATE_LIMITS.lock() {
        if states.len() >= 128 && !states.contains_key(&origin) {
            states.pop_first();
        }
        states.insert(
            origin.clone(),
            RateLimitState {
                origin,
                requests_remaining,
                tokens_remaining,
                requests_reset,
                observed_at: chrono::Utc::now().to_rfc3339(),
            },
        );
    }
}
pub fn rate_limits() -> Vec<RateLimitState> {
    RATE_LIMITS
        .lock()
        .map(|states| states.values().cloned().collect())
        .unwrap_or_default()
}

/// Shared count of HTTP attempts for one model request.
///
/// The adapter records every attempt it really makes; the tool loop reads the
/// value when a stream ends so a clean end-of-stream does not report `unknown`
/// for data the send layer already had. A value of zero means nothing was
/// observed (for example a built-in adapter with its own transport) and stays
/// unknown — it is never defaulted to one.
#[derive(Debug, Clone, Default)]
pub struct AttemptCounter(std::sync::Arc<std::sync::atomic::AtomicU32>);

impl AttemptCounter {
    pub fn set(&self, attempts: u32) {
        self.0.store(attempts, std::sync::atomic::Ordering::Release);
    }

    pub fn attempts(&self) -> Option<u32> {
        match self.0.load(std::sync::atomic::Ordering::Acquire) {
            0 => None,
            attempts => Some(attempts),
        }
    }
}

pub async fn retry_with_backoff<F, Fut, T>(f: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    crate::config::OrchestrationConfig::init_global()?;
    let budget = AttemptBudget::configured();
    retry_with_backoff_budget(f, &budget).await
}

/// Retry one HTTP attempt at a time under a shared budget, so the send layer
/// and the pre-content stream layer cannot multiply the configured maximum.
/// Every failed attempt is wrapped in a sanitized [`RequestFailure`] that keeps
/// the classification and the number of attempts actually made.
pub async fn retry_with_backoff_budget<F, Fut, T>(mut f: F, budget: &AttemptBudget) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    loop {
        let Some(_) = budget.take() else {
            return Err(anyhow::Error::new(RequestFailure::budget_exhausted(
                budget.used(),
            )));
        };
        let started = std::time::Instant::now();
        match f().await {
            Ok(value) => {
                retry_events::emit(budget, "response_received", None, None, false, Some(started.elapsed().as_millis() as u64)).await;
                return Ok(value);
            },
            Err(error) => {
                let attempts = budget.used();
                let retryable = matches!(
                    classify(&error),
                    ErrorClass::RateLimit | ErrorClass::Transient
                );
                let failure = retry_events::safe_failure(&error, attempts);
                if !retryable || budget.remaining() == 0 {
                    retry_events::emit(budget, "failed", Some(failure), None, false, Some(started.elapsed().as_millis() as u64)).await;
                    return Err(anyhow::Error::new(RequestFailure::from_error(
                        &error, attempts,
                    )));
                }
                let retry_after = request_failure::find_in_chain::<ApiError>(&error).and_then(|e| e.retry_after);
                let delay = retry_events::delay(attempts, retry_after);
                retry_events::emit(budget, "retry_wait", Some(failure), Some(delay), retry_after.is_some(), Some(started.elapsed().as_millis() as u64)).await;
                // Dropping the caller cancels this delay and prevents another attempt.
                tokio::time::sleep(delay).await;
            }
        }
    }
}

/// Endpoints known to take a system-role message after the first user
/// message, as the reference CLI sends: DeepSeek (both protocols, verified)
/// and Anthropic. Other servers keep the whole prompt in the system field.
pub fn accepts_mid_system(base_url: &str) -> bool {
    reqwest::Url::parse(base_url).ok().and_then(|url| url.host_str().map(str::to_ascii_lowercase)).is_some_and(|host| {
        host == "deepseek.com" || host.ends_with(".deepseek.com") || host == "api.anthropic.com"
    })
}

/// Index after the first user message, where the reference CLI places its
/// environment message.
fn after_first_user(messages: &[serde_json::Value]) -> Option<usize> {
    messages.iter().position(|message| message["role"] == "user").map(|index| index + 1)
}

/// Anthropic body: keep only a composed prompt's static sections in `system`
/// and send its per-session tail (environment, skills, project) as a system
/// message after the first user message, so `system` is the same for every
/// session and project and shares one cache entry.
pub fn move_system_tail(body: &mut serde_json::Value) {
    let Some((fixed, tail)) = body["system"].as_str().and_then(crate::prompt::split_tail) else { return };
    let Some(at) = body["messages"].as_array().and_then(|messages| after_first_user(messages)) else { return };
    body["system"] = serde_json::json!(fixed);
    body["messages"].as_array_mut().unwrap().insert(at, serde_json::json!({"role":"system","content":tail}));
}

/// Chat Completions body: the same move for a leading system message.
pub fn move_system_tail_chat(body: &mut serde_json::Value) {
    let Some(messages) = body["messages"].as_array_mut() else { return };
    if messages.first().is_none_or(|first| first["role"] != "system") { return }
    let Some((fixed, tail)) = messages[0]["content"].as_str().and_then(crate::prompt::split_tail) else { return };
    let Some(at) = after_first_user(messages) else { return };
    messages[0]["content"] = serde_json::json!(fixed);
    messages.insert(at, serde_json::json!({"role":"system","content":tail}));
}

/// Mark stable prefixes only; no cache of generated answers is created. Composed
/// system prompts get one breakpoint per static section, like the reference CLI.
pub fn apply_prompt_cache(body: &mut serde_json::Value) {
    let ephemeral = || serde_json::json!({"type":"ephemeral"});
    if let Some(system) = body.get("system").and_then(|s| s.as_str()) {
        if let Some(blocks) = crate::prompt::cache_blocks(system) {
            body["system"] = blocks.into_iter().map(|(text, cached)| {
                let mut block = serde_json::json!({"type":"text","text":text});
                if cached { block["cache_control"] = ephemeral(); }
                block
            }).collect();
        } else if system.len() >= 4096 {
            body["system"] = serde_json::json!([{"type":"text","text":system,"cache_control":ephemeral()}]);
        }
    }
}

/// For agent conversations: the system breakpoints plus a rolling breakpoint on
/// the last user block, so the next turn reuses the conversation so far. One-shot
/// requests skip it because a cache write nobody reads only costs more. At most
/// three of the four allowed breakpoints are used.
pub fn apply_conversation_cache(body: &mut serde_json::Value) {
    apply_prompt_cache(body);
    let ephemeral = || serde_json::json!({"type":"ephemeral"});
    let Some(last) = body.get_mut("messages").and_then(|m| m.as_array_mut()).and_then(|m| m.last_mut()) else { return };
    if last["role"] != "user" { return; }
    if let Some(text) = last["content"].as_str().filter(|text| !text.is_empty()).map(str::to_owned) {
        last["content"] = serde_json::json!([{"type":"text","text":text,"cache_control":ephemeral()}]);
    } else if let Some(block) = last["content"].as_array_mut().and_then(|blocks| blocks.iter_mut().rev().find(|block| match block["type"].as_str() {
        // Empty text blocks and thinking blocks reject cache_control.
        Some("text") => block["text"].as_str().is_some_and(|text| !text.is_empty()),
        Some("tool_result" | "image" | "document") => true,
        _ => false,
    })) {
        block["cache_control"] = ephemeral();
    }
}

/// A UTF-8 byte budget including the omission marker, retaining both ends.
pub fn head_tail_preview(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_owned();
    }
    let marker = format!(
        "\n... (output truncated; {} bytes before preview) ...\n",
        text.len()
    );
    if limit < marker.len() {
        return "...".chars().take(limit).collect();
    }
    let budget = limit - marker.len();
    let mut head = budget / 2;
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len() - (budget - budget / 2);
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    format!("{}{}{}", &text[..head], marker, &text[tail..])
}

pub fn retain_tool_output(root: &std::path::Path, output: &str, preview: &str) -> String {
    const LIMIT: usize = 16_000;
    if output == preview && output.len() <= LIMIT {
        return output.to_string();
    }
    let persist = || -> std::io::Result<std::path::PathBuf> {
        use std::io::Write;
        let directory = root.join("tool-outputs");
        std::fs::create_dir_all(&directory)?;
        // Stable paths keep replayed requests identical and avoid duplicate spills.
        let digest = ring::digest::digest(&ring::digest::SHA256, output.as_bytes());
        let key: String = digest
            .as_ref()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let path = directory.join(format!("{key}.txt"));
        if std::fs::read(&path).is_ok_and(|bytes| bytes == output.as_bytes()) {
            return Ok(path);
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)?;
        file.write_all(output.as_bytes())?;
        file.sync_all()?;
        Ok(path)
    };
    match persist() {
        Ok(path) => {
            let note = format!(
                "\nSaved tool result ({} bytes, may already be truncated by the tool): {}",
                output.len(),
                path.display()
            );
            format!(
                "{}{note}",
                head_tail_preview(preview, LIMIT.saturating_sub(note.len()))
            )
        }
        Err(_) => output.to_string(), // Never discard the only complete copy.
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RouterConfig {
    pub enabled: bool,
    pub provider_id: String,
    pub model_id: String,
}
pub fn validate_router(ctx: &crate::services::ServiceContext, config: &RouterConfig) -> Result<()> {
    if config.enabled {
        anyhow::ensure!(
            ctx.paths
                .providers()?
                .iter()
                .any(|p| p.id == config.provider_id
                    && p.enabled
                    && p.models
                        .iter()
                        .any(|m| m.id == config.model_id && m.enabled)),
            "Routing target must be an enabled configured provider and model"
        );
    }
    Ok(())
}
pub fn configure_router(
    ctx: &crate::services::ServiceContext,
    config: &RouterConfig,
) -> Result<()> {
    validate_router(ctx, config)?;
    ctx.save_setting("performance.router", config)
}
pub fn simple_turn(text: &str) -> bool {
    let text = text.trim();
    if text.is_empty()
        || text.chars().count() > 160
        || text.split_whitespace().count() > 28
        || text.lines().count() > 1
        || text.contains('`')
        || text.contains("http")
        || text.contains("www.")
    {
        return false;
    }
    let lower = text.to_lowercase();
    ![
        "debug",
        "implement",
        "refactor",
        "error",
        "analy",
        "design",
        "review",
        "test",
        "plan",
        "tool",
        "shell",
        "write",
        "read",
        "file",
        "delete",
        "代码",
        "修改",
        "文件",
        "删除",
        "分析",
        "实现",
        "测试",
    ]
    .iter()
    .any(|word| lower.contains(word))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn permanent_errors_containing_five_are_not_retried() {
        for status in [400, 401, 403, 404, 422] {
            let error = ApiError {
                status,
                retry_after: None,
                message: "model 5 rejected input 500".into(),
            }
            .into();
            assert!(!matches!(
                classify(&error),
                ErrorClass::Transient | ErrorClass::RateLimit
            ));
        }
        assert_eq!(
            classify(
                &ApiError {
                    status: 503,
                    retry_after: None,
                    message: String::new()
                }
                .into()
            ),
            ErrorClass::Transient
        );
    }
    #[tokio::test]
    async fn permanent_error_runs_once() {
        let count = std::sync::atomic::AtomicUsize::new(0);
        let result: Result<()> = retry_with_backoff(|| {
            count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async { Err(anyhow::anyhow!("invalid model 5")) }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
    #[test]
    fn cache_preserves_system_text() {
        let text = "你好".repeat(1000);
        let mut body = serde_json::json!({"system":text,"messages":[]});
        apply_prompt_cache(&mut body);
        assert_eq!(body["system"][0]["text"], text);
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    }
    #[test]
    fn cache_marks_static_system_blocks_and_the_last_user_block() {
        let system = crate::prompt::compose_system_prompt(Some("rule"), "m", crate::prompt::PromptProfile::Code);
        let mut body = serde_json::json!({"system":system,"messages":[
            {"role":"user","content":"first"},
            {"role":"assistant","content":[{"type":"text","text":"ok"}]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"t","content":"out"},{"type":"text","text":""}]}]});
        apply_conversation_cache(&mut body);
        let blocks = body["system"].as_array().unwrap();
        assert_eq!(blocks.iter().map(|b| b["text"].as_str().unwrap()).collect::<String>().len() + 4, system.len());
        assert_eq!(blocks.iter().map(|b| !b["cache_control"].is_null()).collect::<Vec<_>>(), [true, true, false]);
        assert_eq!(body["messages"][0]["content"], "first");
        assert_eq!(body["messages"][2]["content"][0]["cache_control"]["type"], "ephemeral");
        assert!(body["messages"][2]["content"][1].get("cache_control").is_none());

        // The per-session tail moves after the first user message where accepted.
        let composed = crate::prompt::compose_system_prompt_in(None, "m", crate::prompt::PromptProfile::Code, Some(std::path::Path::new("/w")));
        let (fixed, tail) = crate::prompt::split_tail(&composed).unwrap();
        assert!(tail.starts_with("# Environment") && tail.contains("Primary working directory: /w") && !fixed.contains("Primary working directory"));
        let mut moved = serde_json::json!({"system":composed,"messages":[{"role":"user","content":"hi"},{"role":"assistant","content":"ok"}]});
        move_system_tail(&mut moved);
        assert_eq!(moved["system"], fixed.as_str());
        assert_eq!(moved["messages"][1], serde_json::json!({"role":"system","content":tail}));
        apply_conversation_cache(&mut moved);
        assert!(moved["system"].as_array().unwrap().iter().all(|block| block["cache_control"]["type"] == "ephemeral"));
        let mut chat = serde_json::json!({"messages":[{"role":"system","content":composed},{"role":"user","content":"hi"}]});
        move_system_tail_chat(&mut chat);
        assert_eq!(chat["messages"][0]["content"], fixed.as_str());
        assert_eq!(chat["messages"][2]["role"], "system");
        assert!(accepts_mid_system("https://api.deepseek.com/anthropic") && accepts_mid_system("https://api.anthropic.com"));
        assert!(!accepts_mid_system("https://openrouter.ai/api") && !accepts_mid_system("not a url"));
        let mut body = serde_json::json!({"system":"short","messages":[{"role":"user","content":"hi"}]});
        apply_prompt_cache(&mut body);
        assert_eq!(body["messages"][0]["content"], "hi");
        apply_conversation_cache(&mut body);
        assert_eq!(body["system"], "short");
        assert_eq!(body["messages"][0]["content"], serde_json::json!([{"type":"text","text":"hi","cache_control":{"type":"ephemeral"}}]));
    }
}
