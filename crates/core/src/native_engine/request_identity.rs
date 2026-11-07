//! Task-scoped conversation identity and stable IDs for retried model requests.
use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU32, Ordering};

tokio::task_local! { static TURN: TurnIdentity; }

#[derive(Clone)]
pub(crate) struct TurnIdentity {
    session: Option<String>,
    parent: Option<String>,
    turn: String,
}

impl TurnIdentity {
    pub(crate) fn new(session: Option<String>, parent: Option<String>) -> Self {
        let parent = parent.or_else(|| {
            TURN.try_with(|current| current.session.clone())
                .ok()
                .flatten()
        });
        Self {
            parent: parent.filter(|id| Some(id) != session.as_ref()),
            session,
            turn: uuid::Uuid::new_v4().to_string(),
        }
    }

    pub(crate) async fn scope<F: std::future::Future>(self, future: F) -> F::Output {
        TURN.scope(self, future).await
    }
}

pub(super) fn annotate(snapshot: &mut Value) {
    let _ = TURN.try_with(|identity| {
        snapshot["sessionId"] = json!(identity.session);
        snapshot["threadId"] = json!(identity.session);
        snapshot["parentSessionId"] = json!(identity.parent);
        snapshot["turnId"] = json!(identity.turn);
    });
}

pub(super) struct RequestIdentity {
    headers: HeaderMap,
    attempt: AtomicU32,
}

impl RequestIdentity {
    pub(super) fn id(&self) -> &str {
        self.headers["x-request-id"].to_str().expect("generated request ID")
    }
    pub(super) fn new(capture: Option<&super::request_context::ContextCapture>) -> Self {
        let mut headers = HeaderMap::new();
        let mut set = |name: &'static str, value: &str| {
            // Reject invalid external IDs without failing an otherwise valid request.
            if !value.is_empty() {
                if let Ok(value) = HeaderValue::from_str(value) {
                    headers.insert(name, value);
                }
            }
        };
        let cli = std::env::current_exe().ok().is_some_and(|path| {
            path.file_stem()
                .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("dcl"))
        });
        let app = if cli { "cli" } else { "desktop" };
        let origin = if cli { "dcl" } else { "deepseek-code" };
        set(
            "user-agent",
            &format!(
                "{origin}/{} ({app}; {}; {})",
                env!("CARGO_PKG_VERSION"),
                std::env::consts::OS,
                std::env::consts::ARCH
            ),
        );
        set("x-app", app);
        set("originator", origin);
        set("x-dcl-runtime", "rust");
        set("x-dcl-os", std::env::consts::OS);
        set("x-dcl-arch", std::env::consts::ARCH);
        let request_id = capture
            .and_then(|c| c.event.as_ref())
            .and_then(|(_, snapshot)| snapshot["id"].as_str())
            .map(str::to_owned)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        set("x-request-id", &request_id);
        set("x-client-request-id", &request_id);
        let _ = TURN.try_with(|identity| {
            if let Some(session) = &identity.session {
                for name in [
                    "x-session-id",
                    "session-id",
                    "thread-id",
                    "x-dcl-session-id",
                ] {
                    set(name, session);
                }
            }
            if let Some(parent) = &identity.parent {
                set("x-parent-session-id", parent);
            }
            set("x-dcl-turn-id", &identity.turn);
        });
        Self {
            headers,
            attempt: AtomicU32::new(0),
        }
    }

    pub(super) fn headers(&self) -> HeaderMap {
        let mut headers = self.headers.clone();
        headers.insert(
            "x-dcl-retry-count",
            HeaderValue::from(self.attempt.fetch_add(1, Ordering::Relaxed)),
        );
        headers
    }
}

#[cfg(test)]
#[path = "request_identity_tests.rs"]
mod tests;
