//! Explicit metadata for one model operation, including all its transport retries.
use super::provider_manager::ResolvedProvider;
use reqwest::header::{HeaderMap, HeaderValue};

#[derive(Clone, Debug, Default)]
pub struct RequestOptions {
    pub session_id: Option<String>,
    pub compact: bool,
}

impl RequestOptions {
    pub fn for_session(session_id: Option<&str>) -> Self {
        Self {
            session_id: session_id.map(str::to_owned),
            compact: false,
        }
    }

    pub(crate) fn headers(&self, provider: &ResolvedProvider) -> anyhow::Result<HeaderMap> {
        let mut headers = HeaderMap::new();
        if is_deepseek(provider) {
            if let Some(session) = &self.session_id {
                // Never normalize or silently replace an external session ID.
                headers.insert(
                    "x-deepseek-harness-session-id",
                    HeaderValue::from_str(session)?,
                );
            }
            if self.compact {
                headers.insert("x-deepseek-harness-compact", HeaderValue::from_static("1"));
            }
        }
        Ok(headers)
    }
}

/// Provider identity or an exact DNS suffix, never a model-name substring.
pub fn is_deepseek(provider: &ResolvedProvider) -> bool {
    provider.provider.builtin_id.as_deref() == Some("deepseek")
        || reqwest::Url::parse(provider.provider.base_url.trim())
            .ok()
            .and_then(|url| url.host_str().map(str::to_ascii_lowercase))
            .is_some_and(|host| host == "deepseek.com" || host.ends_with(".deepseek.com"))
}

#[cfg(test)]
#[path = "request_options_tests.rs"]
mod tests;
