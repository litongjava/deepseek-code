//! Structured diagnostics for model requests that fail before, during or after
//! a response. Errors stay typed until this boundary; only sanitized text is
//! exposed to logs, events, session records and hosts.
use super::ErrorClass;
use serde_json::{json, Value};
use std::fmt;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::OnceLock;

/// A long provider message is useful only up to a point; never log a whole body.
const CAUSE_LIMIT: usize = 600;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestStage {
    /// Failure before an HTTP request exists (provider/credential resolution).
    Setup,
    /// The TCP/TLS connection could not be established.
    Connect,
    /// The request was being written or a response was awaited.
    Send,
    /// The service answered with an HTTP error status.
    Response,
    /// The response body could not be transferred or decoded.
    Body,
    /// The streaming response started but did not complete.
    Stream,
}

impl RequestStage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Setup => "setup",
            Self::Connect => "connect",
            Self::Send => "send",
            Self::Response => "response",
            Self::Body => "body",
            Self::Stream => "stream",
        }
    }
}

impl fmt::Display for RequestStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Typed failure retained through the retry layer and the tool loop.
///
/// `source()` intentionally returns `None`: the raw transport chain may contain
/// a URL with query credentials or echoed provider text. `cause` is the only
/// text form, already sanitized and bounded.
#[derive(Debug, Clone)]
pub struct RequestFailure {
    stage: RequestStage,
    class: ErrorClass,
    status: Option<u16>,
    attempts: Option<u32>,
    content_started: Option<bool>,
    cause: String,
}

impl RequestFailure {
    /// Wrap a non-streaming request failure. `attempts` counts HTTP attempts
    /// made for this model request; retries are attempts minus one.
    pub fn from_error(error: &anyhow::Error, attempts: u32) -> Self {
        if let Some(existing) = find_in_chain::<Self>(error) {
            let mut existing = existing.clone();
            existing.attempts = Some(attempts);
            return existing;
        }
        Self {
            stage: stage_of(error),
            class: super::classify_raw(error),
            status: status_of(error),
            attempts: Some(attempts),
            content_started: None,
            cause: sanitized_chain(error),
        }
    }

    /// Wrap a failure observed while a response stream was being consumed.
    /// An existing structured failure keeps its stage: a connection reopened
    /// for a pre-content retry reports `send`, not `stream`.
    pub fn stream(error: &anyhow::Error, attempts: Option<u32>, content_started: bool) -> Self {
        if let Some(existing) = find_in_chain::<Self>(error) {
            let mut existing = existing.clone();
            existing.attempts = existing.attempts.or(attempts);
            existing.content_started =
                Some(existing.content_started.unwrap_or(false) || content_started);
            return existing;
        }
        Self {
            stage: RequestStage::Stream,
            class: super::classify_raw(error),
            status: status_of(error),
            attempts,
            content_started: Some(content_started),
            cause: sanitized_chain(error),
        }
    }

    /// A stream closed before any content or completion marker after bounded retries.
    pub fn stream_exhausted(attempts: u32) -> Self {
        Self {
            stage: RequestStage::Stream,
            class: ErrorClass::Transient,
            status: None,
            attempts: Some(attempts),
            content_started: Some(false),
            cause: format!(
                "stream closed before any content or completion marker after {attempts} attempts"
            ),
        }
    }

    /// The stream stopped without a completion marker and without a transport
    /// error. `attempts` comes from the send layer, which knows how many HTTP
    /// requests were really made; `None` stays unknown instead of defaulting to 1.
    pub fn stream_incomplete(attempts: Option<u32>, content_started: bool) -> Self {
        Self {
            stage: RequestStage::Stream,
            class: ErrorClass::Transient,
            status: None,
            attempts,
            content_started: Some(content_started),
            cause: "provider stream closed without a completion marker".to_owned(),
        }
    }

    /// A shared attempt budget was already spent before this layer could send.
    /// Earlier attempts failed, so `send` describes the exchange that stopped.
    pub fn budget_exhausted(attempts: u32) -> Self {
        Self {
            stage: RequestStage::Send,
            class: ErrorClass::Permanent,
            status: None,
            attempts: Some(attempts),
            content_started: None,
            cause: format!("model request attempt budget exhausted after {attempts} attempts"),
        }
    }

    pub fn stage(&self) -> RequestStage {
        self.stage
    }

    pub fn class(&self) -> ErrorClass {
        self.class
    }

    pub fn status(&self) -> Option<u16> {
        self.status
    }

    pub fn attempts(&self) -> Option<u32> {
        self.attempts
    }

    pub fn retries(&self) -> Option<u32> {
        self.attempts.map(|attempts| attempts.saturating_sub(1))
    }

    pub fn content_started(&self) -> Option<bool> {
        self.content_started
    }

    pub fn cause(&self) -> &str {
        &self.cause
    }

    /// Bounded, machine-parseable single line. `Model API error {status}:` keeps
    /// the established meaning of provider status errors; the trailing bracket
    /// always carries the stage, class, status, attempts and stream boundary.
    pub fn diagnostic(&self) -> String {
        let http = self
            .status
            .map(|status| status.to_string())
            .unwrap_or_else(|| "unknown".to_owned());
        let attempts = self
            .attempts
            .map(|attempts| attempts.to_string())
            .unwrap_or_else(|| "unknown".to_owned());
        let retries = self
            .retries()
            .map(|retries| retries.to_string())
            .unwrap_or_else(|| "unknown".to_owned());
        let content = self
            .content_started
            .map(|started| started.to_string())
            .unwrap_or_else(|| "unknown".to_owned());
        let metadata = format!(
            "[stage={}; class={}; http={}; attempts={}; retries={}; content_started={}]",
            self.stage.as_str(),
            self.class.as_str(),
            http,
            attempts,
            retries,
            content
        );
        match self.status {
            Some(status) => {
                // The cause chain already starts with the provider status; keep
                // one copy so the line reads as a single statement.
                let prefix = format!("Model API error {status}: ");
                let cause = self.cause.strip_prefix(&prefix).unwrap_or(&self.cause);
                format!("Model API error {status}: {cause}{metadata}")
            }
            None => format!("Model request failed: {}{metadata}", self.cause),
        }
    }

    pub fn as_json(&self) -> Value {
        json!({
            "stage": self.stage.as_str(),
            "class": self.class.as_str(),
            "http": self.status,
            "attempts": self.attempts,
            "retries": self.retries(),
            "contentStarted": self.content_started,
            "cause": self.cause,
        })
    }
}

impl fmt::Display for RequestFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.diagnostic())
    }
}

impl std::error::Error for RequestFailure {}

/// Fields parsed from a diagnostic produced by [`RequestFailure::diagnostic`].
/// Hosts use this instead of matching transport phrases in provider text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailureSummary {
    pub stage: String,
    pub class: String,
    pub http: Option<u16>,
    pub attempts: Option<u32>,
    pub retries: Option<u32>,
    pub content_started: Option<bool>,
}

/// Parse the trailing metadata bracket of a host-generated diagnostic.
///
/// Only the last bracket is considered and it must terminate the string, so a
/// provider body cannot fabricate retry information in the middle of a message.
pub fn parse_failure_summary(text: &str) -> Option<FailureSummary> {
    let start = text.rfind("[stage=")?;
    let rest = &text[start..];
    let close = rest.find(']')?;
    let marker = &rest[..=close];
    // The CLI appends "(session <id>)"; nothing else may follow the metadata,
    // so a provider body cannot append a second, fabricated bracket.
    let trailing = rest[close + 1..].trim();
    if !trailing.is_empty()
        && !(trailing.starts_with("(session ")
            && trailing.ends_with(')')
            && !trailing[9..trailing.len() - 1].contains(')'))
    {
        return None;
    }
    let prefix = &text[..start];
    if !prefix.contains("Model request failed") && !prefix.contains("Model API error ") {
        return None;
    }
    let mut stage = None;
    let mut class = None;
    let mut http = None;
    let mut attempts = None;
    let mut retries = None;
    let mut content_started = None;
    for field in marker[1..marker.len() - 1].split(';') {
        let Some((key, value)) = field.split_once('=') else {
            continue;
        };
        match key.trim() {
            "stage" => stage = Some(value.trim().to_owned()),
            "class" => class = Some(value.trim().to_owned()),
            "http" => http = value.trim().parse::<u16>().ok(),
            "attempts" => attempts = value.trim().parse::<u32>().ok(),
            "retries" => retries = value.trim().parse::<u32>().ok(),
            "content_started" => {
                content_started = match value.trim() {
                    "true" => Some(true),
                    "false" => Some(false),
                    _ => None,
                }
            }
            _ => {}
        }
    }
    Some(FailureSummary {
        stage: stage.filter(|stage| !stage.is_empty())?,
        class: class.filter(|class| !class.is_empty())?,
        http,
        attempts,
        retries,
        content_started,
    })
}

/// Find the outermost structured failure anywhere in an error chain.
pub fn find_failure(error: &anyhow::Error) -> Option<&RequestFailure> {
    find_in_chain::<RequestFailure>(error)
}

pub(crate) fn find_in_chain<T: std::error::Error + 'static>(error: &anyhow::Error) -> Option<&T> {
    error.chain().find_map(|cause| cause.downcast_ref::<T>())
}

fn stage_of(error: &anyhow::Error) -> RequestStage {
    if find_in_chain::<super::ApiError>(error).is_some() {
        return RequestStage::Response;
    }
    let Some(http) = find_in_chain::<reqwest::Error>(error) else {
        return RequestStage::Setup;
    };
    if http.is_connect() {
        RequestStage::Connect
    } else if http.is_body() || http.is_decode() {
        RequestStage::Body
    } else {
        RequestStage::Send
    }
}

fn status_of(error: &anyhow::Error) -> Option<u16> {
    find_in_chain::<super::ApiError>(error)
        .map(|api| api.status)
        .or_else(|| {
            find_in_chain::<reqwest::Error>(error)
                .and_then(reqwest::Error::status)
                .map(|status| status.as_u16())
        })
}

/// Join every cause into one sanitized line, dropping duplicate context.
pub fn sanitized_chain(error: &anyhow::Error) -> String {
    let mut parts: Vec<String> = Vec::new();
    for cause in error.chain() {
        // `ApiError.message` is remote text. Sanitizing its Display text is not
        // enough: a server can echo the request body, which no credential rule
        // matches, so the message itself goes through the remote-text policy.
        let text = match cause.downcast_ref::<super::ApiError>() {
            Some(api) => sanitize_remote_text(&api.message),
            None => sanitize_message(&cause.to_string()),
        };
        let text = text.trim().to_owned();
        if text.is_empty() || parts.last().is_some_and(|last| last == &text) {
            continue;
        }
        parts.push(text);
    }
    bound(parts.join(" -> "))
}

/// Remove credentials, URL queries and control characters from one message.
///
/// Header redaction happens first, while line breaks still delimit a header
/// value. Flattening control characters first would merge every following
/// header value into the first one and let the rest of a multi-value header
/// survive, so the order here is part of the guarantee.
pub fn sanitize_message(text: &str) -> String {
    let text = redact_header_lines(text);
    let text = flatten_controls(&text);
    let text = redact_inline_headers(&text);
    let text = scrub_urls(&text);
    let text = crate::services::feedback::redact(&text, &[]);
    bound(text.trim().to_owned())
}

/// Remote text that must never be trusted as-is: server error messages can echo
/// the request that produced them. Returns a conservative note instead.
pub fn sanitize_remote_text(text: &str) -> String {
    let sanitized = sanitize_message(text);
    if carries_request_structure(&sanitized) {
        return "remote details omitted because they appear to echo the request".to_owned();
    }
    sanitized
}

/// A server can quote the request it rejected inside any error field. Free text
/// cannot be proven free of such an echo, so any JSON structure — an object, an
/// array, or a quoted key followed by a colon with any spacing — is refused
/// outright. Keyword lists miss variants such as `"input" : "..."` and would
/// keep leaking; structure is what an echoed payload always carries.
fn carries_request_structure(text: &str) -> bool {
    static KEY: OnceLock<regex::Regex> = OnceLock::new();
    // Redaction placeholders are literal brackets and must not read as JSON.
    let text = text.replace("[REDACTED]", "");
    if text.contains('{') || text.contains('}') || text.contains('[') || text.contains(']') {
        return true;
    }
    KEY.get_or_init(|| {
        regex::Regex::new(r#"["'][A-Za-z_][A-Za-z0-9_.-]*["']\s*:"#)
            .expect("constant JSON key pattern")
    })
    .is_match(&text)
}

fn flatten_controls(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

/// Byte-for-byte replacement is only safe for delimiters, not for values.
fn redact_header_lines(text: &str) -> String {
    text.split('\n')
        .map(|line| {
            let body = line.strip_suffix('\r').unwrap_or(line);
            if header_name(body).is_some_and(|name| is_sensitive_header(&name)) {
                "[REDACTED]".to_owned()
            } else {
                body.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn header_name(line: &str) -> Option<String> {
    let (name, _) = line.trim_start().split_once(':')?;
    let name = name.trim().trim_matches('"').to_ascii_lowercase();
    (!name.is_empty() && name.len() <= 40).then_some(name)
}

/// Every header whose value is a credential or identifies the caller.
fn is_sensitive_header(name: &str) -> bool {
    matches!(
        name,
        "cookie"
            | "set-cookie"
            | "authorization"
            | "proxy-authorization"
            | "proxy-authenticate"
            | "x-api-key"
            | "api-key"
    )
}

/// A cookie value may contain `;` and an Authorization value may contain a
/// scheme plus spaces, so the whole value goes, not just its first token.
fn redact_inline_headers(text: &str) -> String {
    static QUOTED: OnceLock<regex::Regex> = OnceLock::new();
    static REST: OnceLock<regex::Regex> = OnceLock::new();
    let quoted = QUOTED.get_or_init(|| {
        regex::Regex::new(
            r#"(?i)\b(?:set-cookie|cookie|proxy-authorization|authorization|x-api-key)\s*["']?\s*[:=]\s*"[^"]*""#,
        )
        .expect("constant header pattern")
    });
    let rest = REST.get_or_init(|| {
        regex::Regex::new(
            r"(?i)\b(?:set-cookie|cookie|proxy-authorization|authorization|x-api-key)\s*[^:\r\n]{0,20}[:=]\s*[^\r\n]*",
        )
        .expect("constant header pattern")
    });
    let text = quoted.replace_all(text, "[REDACTED]").into_owned();
    rest.replace_all(&text, "[REDACTED]").into_owned()
}

fn scrub_urls(text: &str) -> String {
    static URL: OnceLock<regex::Regex> = OnceLock::new();
    let pattern = URL.get_or_init(|| {
        regex::Regex::new(r#"(?i)https?://[^\s)\]}>\"'<,]+"#).expect("constant URL pattern")
    });
    pattern
        .replace_all(text, |captures: &regex::Captures<'_>| {
            // Keep only scheme, host and port: paths and queries can carry tokens.
            let raw = captures
                .get(0)
                .map(|value| value.as_str())
                .unwrap_or_default();
            match reqwest::Url::parse(raw) {
                Ok(url) => {
                    let host = url.host_str().unwrap_or("[redacted-host]");
                    match url.port() {
                        Some(port) => format!("{}://{host}:{port}", url.scheme()),
                        None => format!("{}://{host}", url.scheme()),
                    }
                }
                Err(_) => "[redacted-url]".to_owned(),
            }
        })
        .into_owned()
}

fn bound(text: String) -> String {
    if text.chars().count() <= CAUSE_LIMIT {
        return text;
    }
    let mut bounded: String = text.chars().take(CAUSE_LIMIT).collect();
    bounded.push('\u{2026}');
    bounded
}

/// Limits the total number of HTTP attempts shared by nested retry layers.
///
/// A model request must not multiply attempts when the send layer and the
/// pre-content stream layer both retry. Every HTTP request takes one unit;
/// `attempts` is the number taken, `retries` is that minus one.
#[derive(Debug)]
pub struct AttemptBudget {
    pub(crate) id: String,
    pub(super) events: Option<tokio::sync::mpsc::Sender<crate::native_engine::tool_loop::EngineEvent>>,
    max: u32,
    used: AtomicU32,
}

impl AttemptBudget {
    pub fn configured() -> Self {
        Self::new(crate::config::OrchestrationConfig::max_retries().saturating_add(1))
    }

    pub fn new(max: u32) -> Self {
        Self {
            max: max.max(1),
            id: uuid::Uuid::new_v4().to_string(),
            events: super::retry_events::sender(),
            used: AtomicU32::new(0),
        }
    }

    /// Take the next 0-based attempt index, or `None` when the budget is spent.
    pub fn take(&self) -> Option<u32> {
        let mut current = self.used.load(Ordering::Relaxed);
        loop {
            if current >= self.max {
                return None;
            }
            match self.used.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Some(current),
                Err(actual) => current = actual,
            }
        }
    }

    pub fn used(&self) -> u32 {
        self.used.load(Ordering::Acquire)
    }

    pub fn max(&self) -> u32 {
        self.max
    }

    pub fn remaining(&self) -> u32 {
        self.max.saturating_sub(self.used())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_causes_are_sanitized_and_url_queries_are_dropped() {
        let error = anyhow::anyhow!(
            "error sending request for url (https://user:secret@api.example.com:8443/v1/messages?api_key=query-secret&x=1)"
        )
        .context("Authorization: Bearer sk-ant-abcdefghijklmnop")
        .context("Cookie: session=cookie-secret");
        let cause = sanitized_chain(&error);
        for secret in [
            "secret",
            "query-secret",
            "sk-ant-abcdefghijklmnop",
            "cookie-secret",
        ] {
            assert!(!cause.contains(secret), "{secret}: {cause}");
        }
        // Scheme, host and a non-default port are kept; default ports are omitted.
        assert!(cause.contains("https://api.example.com:8443"), "{cause}");
        assert!(!cause.contains("/v1/messages"), "{cause}");
    }

    #[test]
    fn diagnostic_metadata_round_trips_and_cannot_be_faked_inside_a_body() {
        let failure = RequestFailure {
            stage: RequestStage::Stream,
            class: ErrorClass::Transient,
            status: None,
            attempts: Some(3),
            content_started: Some(true),
            cause: "body [stage=send; class=permanent] trailing".into(),
        };
        let summary = parse_failure_summary(&failure.diagnostic()).unwrap();
        assert_eq!(summary.stage, "stream");
        assert_eq!(summary.class, "transient");
        assert_eq!(summary.attempts, Some(3));
        assert_eq!(summary.retries, Some(2));
        assert_eq!(summary.content_started, Some(true));

        let provider_text = "Model request failed: body [stage=send; class=transient] trailing";
        assert_eq!(parse_failure_summary(provider_text), None);
        assert_eq!(parse_failure_summary("error sending request"), None);
    }

    #[test]
    fn budget_bounds_nested_layers() {
        let budget = AttemptBudget::new(3);
        assert_eq!(budget.take(), Some(0));
        assert_eq!(budget.take(), Some(1));
        assert_eq!(budget.take(), Some(2));
        assert_eq!(budget.take(), None);
        assert_eq!(budget.used(), 3);
        assert_eq!(budget.remaining(), 0);
        assert_eq!(AttemptBudget::new(0).max(), 1);
    }
}
