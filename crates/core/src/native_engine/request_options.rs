//! Explicit metadata for one model operation, including all its transport retries.
use super::provider_manager::ResolvedProvider;
use reqwest::header::{HeaderMap, HeaderValue};

#[derive(Clone, Debug, Default)]
pub struct RequestOptions {
    pub session_id: Option<String>,
    pub compact: bool,
    pub recording: Option<super::request_recording::RecordingContext>,
    pub purpose: String,
    pub compaction_stage: Option<String>,
    pub compaction_label: Option<String>,
    pub model_request_id: Option<String>,
    pub(crate) compaction_record: Option<(
        std::path::PathBuf,
        std::sync::Arc<std::sync::Mutex<Vec<u64>>>,
    )>,
}

impl RequestOptions {
    pub fn for_session(session_id: Option<&str>) -> Self {
        Self {
            session_id: session_id.map(str::to_owned),
            purpose: "main".into(),
            ..Default::default()
        }
    }

    pub fn from_root(root: &std::path::Path, session: Option<&str>, purpose: &str) -> Self {
        let mut options = Self::for_session(session).with_purpose(purpose);
        if session.is_some() && super::request_recording::Policy::default().enabled_at(root) {
            options.recording = Some(super::request_recording::RecordingContext {
                root: root.into(),
                parent_session_id: None,
            });
        }
        options
    }

    pub fn from_db(db: &crate::db::DbManager, session: Option<&str>, purpose: &str) -> Self {
        let mut options = Self::for_session(session).with_purpose(purpose);
        if let Some(session) = session {
            let parent = recording_parent(db, session);
            let mut policy = db.request_recording_policy(session);
            let mut ancestor = parent.clone();
            let mut visited = std::collections::HashSet::from([session.to_owned()]);
            while policy.is_none() {
                let Some(id) = ancestor else {
                    break;
                };
                if !visited.insert(id.clone()) {
                    break;
                }
                policy = db.request_recording_policy(&id);
                ancestor = recording_parent(db, &id);
            }
            let policy = policy.unwrap_or_default();
            if policy.enabled_at(db.data_dir()) {
                if purpose == "main" && parent.is_some() {
                    options.purpose = "subagent".into();
                }
                options.recording = Some(super::request_recording::RecordingContext {
                    root: db.data_dir().into(),
                    parent_session_id: parent,
                });
            }
        }
        options
    }

    pub fn with_purpose(mut self, purpose: &str) -> Self {
        self.purpose = purpose.into();
        self
    }

    pub fn compaction(mut self, stage: &str) -> Self {
        self.compact = true;
        self.purpose = "compaction".into();
        self.compaction_label = Some(stage.into());
        self.compaction_stage = Some(
            if stage.contains("split")
                || stage.contains("retrying")
                || stage.contains("raised")
                || stage.contains(" merge")
            {
                "recovery"
            } else if stage.starts_with("fragment") {
                "segment"
            } else if stage.starts_with("merge") {
                "merge"
            } else if stage.starts_with("final") {
                "final"
            } else {
                stage
            }
            .into(),
        );
        self
    }

    pub(crate) fn observed(
        mut self,
        capture: Option<&super::request_context::ContextCapture>,
    ) -> Self {
        if let Some(id) = capture
            .and_then(|c| c.event.as_ref())
            .and_then(|(_, snapshot)| snapshot["id"].as_str())
        {
            self.model_request_id = Some(id.to_owned());
        }
        self
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

fn recording_parent(db: &crate::db::DbManager, session: &str) -> Option<String> {
    db.with_conn(|conn| {
        conn.query_row(
            "SELECT value FROM peripheral_settings WHERE key=?1",
            [format!("delegation.{session}")],
            |r| r.get::<_, String>(0),
        )
        .ok()
    })
    .ok()
    .flatten()
    .and_then(|v| serde_json::from_str::<serde_json::Value>(&v).ok())
    .and_then(|v| v["parent_session_id"].as_str().map(str::to_owned))
    .or_else(|| {
        crate::services::background_sessions::worker_directory(db.data_dir(), session)
            .ok()
            .flatten()
            .and_then(|dir| crate::services::background_sessions::read_job(&dir).ok())
            .map(|job| job.parent_session_id)
    })
    .filter(|p| p != session)
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
