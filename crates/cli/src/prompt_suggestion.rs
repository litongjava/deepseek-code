//! Terminal and SDK presentation of the latest suggestion for a session.
use deepseek_code_core::{
    db::session_store::SessionDb,
    native_engine::suggestion::SuggestionCapture,
    session_files::suggestion::{SavedSuggestion, SuggestionGuard},
};
use serde_json::Value;
use std::{path::Path, sync::Arc};

pub fn enabled(data_dir: &Path) -> bool {
    std::fs::read_to_string(
        deepseek_code_core::data_layout::DataLayout::from_root(&data_dir)
            .config_file("settings.json"),
    )
    .ok()
    .and_then(|text| serde_json::from_str::<Value>(&text).ok())
    .is_none_or(|settings| settings["promptSuggestionEnabled"] != Value::Bool(false))
}

/// Drops empty, meta, labelled or otherwise unusable suggestions.
pub fn filter(text: &str) -> Option<String> {
    let suggestion = text
        .trim()
        .trim_matches(|c| c == '"' || c == '\'' || c == '`')
        .trim();
    let lower = suggestion.to_lowercase();
    let words = suggestion.split_whitespace().count();
    let single_word_ok = suggestion.starts_with('/')
        || [
            "yes", "ok", "okay", "sure", "continue", "push", "commit", "deploy", "stop", "exit",
        ]
        .contains(&lower.as_str())
        || suggestion.chars().any(|c| !c.is_ascii());
    let meta = lower == "done"
        || lower.starts_with("nothing")
        || lower.starts_with("no suggestion")
        || lower.contains("silent")
        || lower.contains("silence");
    let rejected = suggestion.is_empty()
        || suggestion.contains('\n')
        || suggestion.chars().count() > 120
        || meta
        || (suggestion.starts_with('(') && suggestion.ends_with(')'))
        || (suggestion.starts_with('[') && suggestion.ends_with(']'))
        || suggestion.split_once(": ").is_some_and(|(label, _)| {
            !label.contains(' ') && label.chars().all(char::is_alphanumeric)
        })
        || (words < 2 && !single_word_ok)
        || words > 12
        || ["let me", "i'll", "i will", "here's", "here is"]
            .iter()
            .any(|start| lower.starts_with(start));
    (!rejected).then(|| suggestion.to_owned())
}

pub(crate) fn foreground_worker(session: &crate::session::Session) -> bool {
    use deepseek_code_core::services::background_sessions as jobs;
    let Some(directory) = std::env::var_os(jobs::CONTROL_ENV) else {
        return false;
    };
    Path::new(&directory)
        .file_name()
        .and_then(|id| id.to_str())
        .and_then(|id| jobs::get(&session.paths.data_dir, id).ok())
        .is_some_and(|job| {
            job.foreground
                && job.session_id == session.id
                && jobs::authorize_job(&session.paths.data_dir, &job, &session.id).is_ok()
        })
}

pub(crate) struct Pending {
    pub capture: SuggestionCapture,
    pub guard: Arc<SuggestionGuard>,
}

impl Pending {
    pub async fn generate(self) -> anyhow::Result<Option<String>> {
        let text = self.capture.generate(self.guard.clone()).await?;
        let Some(text) = text.as_deref().and_then(filter) else {
            return Ok(None);
        };
        self.guard.save(text)
    }
}

#[derive(Clone)]
pub(crate) struct Ghost {
    db: SessionDb,
    pub saved: SavedSuggestion,
}

impl Ghost {
    pub fn load(session: &crate::session::Session) -> Option<Self> {
        let db = session.selected_session().ok()?;
        let saved = SavedSuggestion::load_from(&db).ok()??;
        Some(Self {
            db,
            saved,
        })
    }

    pub fn accept(&self) -> bool {
        self.saved.accept_from(&self.db).unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggestions_are_filtered_like_the_reference_cli() {
        assert_eq!(
            filter("\"run the tests\"").as_deref(),
            Some("run the tests")
        );
        assert_eq!(filter("commit this").as_deref(), Some("commit this"));
        assert_eq!(filter("yes").as_deref(), Some("yes"));
        assert_eq!(filter("/compact").as_deref(), Some("/compact"));
        assert_eq!(filter("提交并推送").as_deref(), Some("提交并推送"));
        for rejected in [
            "",
            "done",
            "Nothing to suggest",
            "(silence)",
            "Suggestion: run it",
            "great",
            "Let me run the tests",
            "one\ntwo",
        ] {
            assert!(filter(rejected).is_none(), "{rejected:?}");
        }
    }
}
