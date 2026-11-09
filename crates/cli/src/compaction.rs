use crate::{args::Cli, config};
use anyhow::Result;
use deepseek_code_core::{
    cache::FileCache,
    native_engine::provider_manager::ProviderManager,
    runtime::{self, RuntimePaths, SessionStore},
};
use std::sync::Arc;
use tokio::sync::Mutex;

pub async fn run(cli: &Cli, paths: &RuntimePaths, id: &str, instructions: &str) -> Result<()> {
    let db = paths.open_database()?;
    let store = SessionStore::new(db.clone());
    let session = store.resume(id)?;
    let workspace = store.workspace(id, &paths.data_dir.join("workspaces"))?;
    let context = crate::services::context(paths)?;
    let selected = config::select(
        cli,
        paths,
        session.model.as_deref(),
        session.provider.as_deref(),
    )?;
    deepseek_code_core::config::OrchestrationConfig::init_global()?;
    let manager =
        ProviderManager::in_memory(selected.providers, paths.data_dir.join("providers.json"));
    let engine = runtime::create_engine(
        paths,
        db.clone(),
        Arc::new(Mutex::new(manager)),
        runtime::default_permissions(),
        Arc::new(FileCache::new()),
    );
    tokio::select! {
        result = crate::hook_progress::during(id, true, deepseek_code_core::services::compaction::compact_with_hooks(&engine, &context, &workspace, id, &selected.provider_id, &selected.model, instructions, "manual")) => crate::services::print(&result?),
        _ = tokio::signal::ctrl_c() => anyhow::bail!("Compaction interrupted; check /history for a completed summary. Original transcript retained"),
    }
}

/// Persist notices for detached workers so an attaching frontend can replay them.
pub(crate) fn notice(message: &str) {
    let event = serde_json::json!({"type":"notice", "message":message});
    if !matches!(
        deepseek_code_core::services::session_worker::emit_event(&event),
        Ok(true)
    ) {
        eprintln!("{message}");
    }
}

/// Completion details shared by the manual, automatic, overflow-retry and
/// print entries: the count, the summary path on its own line, and an export
/// failure. The duration stays with each entry's first line.
pub(crate) fn completion_details(
    report: &deepseek_code_core::services::compaction::CompactionResult,
) -> Vec<String> {
    let mut lines = vec![format!(
        "{} messages summarized; the original transcript remains in /history.",
        report.summarized_messages
    )];
    if let Some(path) = &report.summary_path {
        lines.push("Summary saved to:".into());
        lines.push(path.clone());
    }
    if let Some(error) = &report.export_error {
        lines.push(format!("Summary export failed: {error}"));
    }
    lines
}

/// One-line-per-detail report for entries without their own layout.
pub(crate) fn completion_report(
    report: &deepseek_code_core::services::compaction::CompactionResult,
) -> String {
    format!(
        "Compacted in {}: {}",
        crate::repl::format_duration(report.duration_ms),
        completion_details(report).join("\n")
    )
}

#[cfg(test)]
mod tests {
    use deepseek_code_core::services::compaction::CompactionResult;

    fn report(path: Option<&str>, error: Option<&str>) -> CompactionResult {
        CompactionResult {
            message_id: "message".into(),
            summarized_messages: 2,
            warnings: Vec::new(),
            summary_path: path.map(str::to_owned),
            export_error: error.map(str::to_owned),
            duration_ms: 65_000,
        }
    }

    #[test]
    fn completion_details_keep_the_path_on_its_own_line() {
        let lines = super::completion_details(&report(Some("/tmp/a b/2026.md"), None));
        assert_eq!(
            lines,
            [
                "2 messages summarized; the original transcript remains in /history.",
                "Summary saved to:",
                "/tmp/a b/2026.md",
            ]
        );
        assert_eq!(
            super::completion_report(&report(Some("/tmp/a b/2026.md"), None)),
            "Compacted in 1m 5s: 2 messages summarized; the original transcript remains in /history.\nSummary saved to:\n/tmp/a b/2026.md"
        );
        let failed = super::completion_details(&report(None, Some("read-only")));
        assert_eq!(failed.last().unwrap(), "Summary export failed: read-only");
    }

    #[test]
    fn compaction_notices_are_retained_in_the_worker_event_log() {
        use deepseek_code_core::services::session_worker::set_event_sink;
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("events.jsonl");
        std::fs::write(&path, "").unwrap();
        set_event_sink(Some(path.clone()));
        super::notice("Auto-compacting conversation");
        super::notice("Hook: expected failure");
        set_event_sink(None);
        let events: Vec<serde_json::Value> = std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["type"], "notice");
        assert_eq!(events[0]["message"], "Auto-compacting conversation");
        assert_eq!(events[1]["message"], "Hook: expected failure");
    }
}
