//! Lifecycle dispatch shared by standalone hosts and the native engine.
use crate::db::session_store::{SessionAccess,settings as saved};
use super::{
    tool_hooks::{HookEvent, HookOutcome, HookRunner},
    ServiceContext,
};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
};

/// Resolve the advertised transcript location without accessing storage.
pub(crate) fn transcript_path(root: &Path, session: &str) -> PathBuf {
    use sha2::{Digest,Sha256};
    let name = if session.len() > 100 {
        format!("hash-{:x}",Sha256::digest(session.as_bytes()))
    } else {
        session.as_bytes().iter().map(|b| format!("{b:02x}")).collect()
    };
    crate::data_layout::DataLayout::from_root(root)
        .hook_transcripts()
        .join(format!("{name}.jsonl"))
}

/// A real, readable JSONL snapshot of the durable conversation (no request headers).
pub(crate) fn transcript(root: &Path, session: &str) -> Result<PathBuf> {
    snapshot(root, session, None)
}

pub(crate) fn snapshot_path(root: &Path, session: &str, history: Option<&crate::db::session_store::ExecutorHistory>) -> PathBuf {
    if let Some(crate::db::session_store::ExecutorHistory::Session(s)) = history {
        crate::data_layout::DataLayout::from_root(root)
            .hook_transcripts()
            .join(format!("{}.jsonl",s.incarnation()))
    } else {
        transcript_path(root, session)
    }
}

pub(crate) fn snapshot(root:&Path, session:&str, bound:Option<&crate::db::session_store::ExecutorHistory>) -> Result<PathBuf> {
    use crate::db::session_store::Stores;
    let selected;
    let history = if let Some(bound) = bound {
        anyhow::ensure!(bound.id()==session,"Hook session does not match its bound history");
        Some(bound)
    } else if root.join("global.db").try_exists()? {
        let stores = Stores::open(root)?;
        selected = stores.listed_history(session)?;
        selected.as_ref()
    } else { None };
    let path = snapshot_path(root, session, history);
    let directory = crate::data_layout::DataLayout::from_root(root).hook_transcripts();
    std::fs::create_dir_all(&directory)?;
    let mut snapshot = tempfile::NamedTempFile::new_in(&directory)?;
    if let Some(history) = history {
        history.write_hook_transcript(&mut snapshot)?;
    }
    snapshot.persist(&path).map_err(|e| e.error)?;
    Ok(path)
}

pub async fn run(
    context: &ServiceContext,
    workspace: &Path,
    session: &str,
    event: HookEvent,
    payload: Value,
) -> Result<HookOutcome> {
    HookRunner::load_with_plugins(context, workspace)?
        .with_session_callbacks(session)
        .run(event, session, payload)
        .await
}

pub async fn setup(
    context: &ServiceContext,
    workspace: &Path,
    session: &str,
    trigger: &str,
    synchronous: bool,
) -> Result<Vec<String>> {
    anyhow::ensure!(
        ["init", "maintenance"].contains(&trigger),
        "Invalid Setup trigger"
    );
    let mut runner =
        HookRunner::load_with_plugins(context, workspace)?.with_session_callbacks(session);
    if synchronous {
        runner.force_synchronous();
    }
    let result = runner
        .run(HookEvent::Setup, session, json!({"trigger":trigger}))
        .await?;
    let mut text = super::session_hooks::context(context, session)?;
    for additional in result.additional_context {
        text.push('\n');
        text.push_str(&additional);
    }
    context.db.select_session(session)?.set_preference(saved::SESSION_START_CONTEXT,Some(&text))?;
    Ok(result.warnings)
}

/// The configuration baseline to seal with a new session before it becomes visible.
pub struct NewSessionConfiguration {
    pub(crate) workspace: PathBuf,
    pub(crate) files: Vec<Option<String>>,
}

fn configuration_files(
    context: &ServiceContext,
    workspace: &Path,
) -> Result<([(&'static str, PathBuf); 3], Vec<Option<String>>)> {
    let sources = [
        (
            "user_settings",
            context.paths.layout().config_file("settings.json"),
        ),
        ("project_settings", workspace.join(".claude/settings.json")),
        (
            "local_settings",
            workspace.join(".claude/settings.local.json"),
        ),
    ];
    let mut current = Vec::new();
    for (_, path) in &sources {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e).with_context(|| format!("Reading {}", path.display())),
        };
        current.push(text);
    }
    Ok((sources, current))
}

pub fn prepare_new_configuration(
    context: &ServiceContext,
    workspace: &Path,
) -> Result<NewSessionConfiguration> {
    let (sources, files) = configuration_files(context, workspace)?;
    let mut configs = super::plugins::command_hooks(context)?;
    for ((_, path), content) in sources.into_iter().zip(&files) {
        if let Some(content) = content {
            let value = serde_json::from_str(content.trim_start_matches('\u{feff}'))
                .with_context(|| format!("Invalid hook settings: {}", path.display()))?;
            configs.push((path, value));
        }
    }
    HookRunner::from_configs(&context.paths.data_dir, workspace, configs)?;
    Ok(NewSessionConfiguration { workspace: workspace.to_owned(), files })
}

/// Check before consuming changed settings. Keep the accepted hook definitions so a
/// changed file cannot remove its own ConfigChange guard. A denied change fails closed
/// until corrected; the original settings file is never rewritten.
pub async fn check_configuration(
    context: &ServiceContext,
    workspace: &Path,
    session: &str,
) -> Result<()> {
    let (sources, current) = configuration_files(context, workspace)?;
    let repo = context.db.select_session(session)?;
    if let Some((rejected, reason)) =
        repo.preference(saved::rejected_configuration(workspace))?
    {
        if rejected == current {
            anyhow::bail!("{reason}");
        }
    }
    if let Some(previous) = repo.preference(saved::accepted_configuration(workspace))? {
        if previous != current {
            let mut configs = super::plugins::command_hooks(context)?;
            for ((_, path), text) in sources.iter().zip(&previous) {
                if let Some(text) = text {
                    configs.push((
                        path.clone(),
                        serde_json::from_str(text.trim_start_matches('\u{feff}'))?,
                    ));
                }
            }
            let runner = HookRunner::from_configs(&context.paths.data_dir, workspace, configs)?
                .with_session_callbacks(session);
            for (index, (source, path)) in sources.iter().enumerate() {
                if previous.get(index) == current.get(index) {
                    continue;
                }
                let outcome = runner
                    .run(
                        HookEvent::ConfigChange,
                        session,
                        json!({"source":source,"file_path":path}),
                    )
                    .await?;
                if outcome.blocked {
                    let reason = format!("ConfigChange blocked {}: {}. Restore or correct the settings before continuing.", path.display(), outcome.reason);
                    repo.set_preference(saved::rejected_configuration(workspace),Some(&(current, reason.clone())))?;
                    anyhow::bail!("{reason}");
                }
            }
        } else {
            repo.set_preference(saved::rejected_configuration(workspace),None)?;
            return Ok(());
        }
    }
    // Validate before accepting, including event/type restrictions.
    HookRunner::load_with_plugins(context, workspace)?;
    repo.set_preference(saved::rejected_configuration(workspace),None)?;
    repo.set_preference(saved::accepted_configuration(workspace), Some(&current))
}

pub async fn instructions(
    context: &ServiceContext,
    runner: &HookRunner,
    session: &str,
    files: &[(PathBuf, String)],
) -> Result<()> {
    let repo=context.db.select_session(session)?;
    let mut seen=repo.preference(saved::SEEN_INSTRUCTIONS)?.unwrap_or_default();
    let boundary=repo.latest_compact_message()?;
    let previous_boundary=repo.preference(saved::INSTRUCTIONS_BOUNDARY)?;
    let compact = boundary.is_some() && boundary != previous_boundary;
    if compact {
        seen.clear();
    }
    let reason = if compact { "compact" } else { "session_start" };
    seen.retain(|(path, _)| files.iter().any(|(loaded, _)| loaded == path));
    for (path, fingerprint) in files {
        if seen
            .iter()
            .any(|(loaded, digest)| loaded == path && digest == fingerprint)
        {
            continue;
        }
        let memory_type = if path
            .file_name()
            .is_some_and(|name| name == "CLAUDE.local.md")
        {
            "Local"
        } else {
            "Project"
        };
        let outcome = runner
            .run(
                HookEvent::InstructionsLoaded,
                session,
                json!({"file_path":path,"memory_type":memory_type,"load_reason":reason}),
            )
            .await?;
        for warning in outcome.warnings {
            tracing::warn!("InstructionsLoaded: {warning}");
        }
        seen.retain(|(loaded, _)| loaded != path);
        seen.push((path.clone(), fingerprint.clone()));
        repo.set_preference(saved::SEEN_INSTRUCTIONS,Some(&seen))?;
    }
    repo.set_preference(saved::SEEN_INSTRUCTIONS,Some(&seen))?;
    repo.set_preference(saved::INSTRUCTIONS_BOUNDARY,boundary.as_ref())?;
    Ok(())
}

pub fn api_error_kind(error: &str) -> &'static str {
    // Classify the transport status, never numbers supplied by the server body.
    // The structured metadata is host-generated; the legacy prefix remains for
    // diagnostics produced by older builds.
    let status = crate::performance::parse_failure_summary(error)
        .and_then(|failure| failure.http)
        .or_else(|| {
            error
                .split_once("Model API error ")
                .and_then(|(_, tail)| tail.split(':').next())
                .and_then(|status| status.trim().parse::<u16>().ok())
        });
    match status {
        Some(401 | 403) => "authentication_failed",
        Some(402) => "billing_error",
        Some(429) => "rate_limit",
        Some(500..=599) => "server_error",
        Some(400..=499) => "invalid_request",
        _ => "unknown",
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn instructions_emit_again_after_real_compaction_boundary_but_not_resume() {
        let dir = tempfile::tempdir().unwrap();
        let context = super::super::session_messages::context(dir.path()).unwrap();
        let store = crate::runtime::SessionStore::new(context.db.clone());
        let session = store.create(dir.path(), "fixture", "fixture").unwrap();
        let file = dir.path().join("CLAUDE.md");
        std::fs::write(&file, "fixture").unwrap();
        let script = dir.path().join("instructions.cjs");
        std::fs::write(&script, r#"const fs=require('fs');let s='';process.stdin.on('data',x=>s+=x);process.stdin.on('end',()=>fs.appendFileSync('instructions.jsonl',JSON.stringify(JSON.parse(s))+'\n'));"#).unwrap();
        crate::data_layout::write_config_file(crate::data_layout::DataLayout::from_root(&dir.path()).config_file("settings.json"), json!({"hooks":{"InstructionsLoaded":[{"hooks":[{"type":"command","command":format!("node \"{}\"",script.display())}]}]}}).to_string()).unwrap();
        let runner = HookRunner::load(dir.path(), dir.path()).unwrap();
        for _ in 0..2 {
            instructions(
                &context,
                &runner,
                &session,
                &[(file.clone(), "fixture".into())],
            )
            .await
            .unwrap();
        }
        context
            .db
            .fixture_history_write(&session, |conn| {
                crate::db::message_repo::insert_message(
                    conn,
                    "boundary",
                    &session,
                    "user",
                    "summary",
                    None,
                    "2026-10-18T00:00:00Z",
                    true,
                    1,
                )
            })
            .unwrap();
        for _ in 0..2 {
            instructions(
                &context,
                &runner,
                &session,
                &[(file.clone(), "fixture".into())],
            )
            .await
            .unwrap();
        }
        let rows: Vec<Value> = std::fs::read_to_string(dir.path().join("instructions.jsonl"))
            .unwrap()
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        assert_eq!(rows.len(), 2);
        for (row, reason) in rows.iter().zip(["session_start", "compact"]) {
            assert_eq!(
                *row,
                json!({"hook_event_name":"InstructionsLoaded","session_id":session,"cwd":dir.path(),"transcript_path":transcript(dir.path(),&session).unwrap(),"file_path":file,"memory_type":"Project","load_reason":reason})
            );
        }
    }

    #[tokio::test]
    async fn settings_change_is_audited_once_and_cannot_remove_its_guard() {
        let dir = tempfile::tempdir().unwrap();
        let context = super::super::session_messages::context(dir.path()).unwrap();
        context.db.create("session",Default::default()).unwrap();
        let script = dir.path().join("guard.cjs");
        std::fs::write(&script, r#"const fs=require('fs');let s='';process.stdin.on('data',x=>s+=x);process.stdin.on('end',()=>{fs.appendFileSync('config-events.jsonl',JSON.stringify(JSON.parse(s))+'\n');console.log(JSON.stringify({decision:'block',reason:'fixture rejection'}))});"#).unwrap();
        let settings =
            crate::data_layout::DataLayout::from_root(&dir.path()).config_file("settings.json");
        let initial = json!({"hooks":{"ConfigChange":[{"matcher":"^user_settings$","hooks":[{"type":"command","command":format!("node \"{}\"",script.display())}]}]}}).to_string();
        crate::data_layout::write_config_file(&settings, &initial).unwrap();
        check_configuration(&context, dir.path(), "session")
            .await
            .unwrap();
        assert!(!dir.path().join("config-events.jsonl").exists());
        crate::data_layout::write_config_file(&settings, "{}").unwrap();
        for _ in 0..2 {
            assert!(check_configuration(&context, dir.path(), "session")
                .await
                .unwrap_err()
                .to_string()
                .contains("fixture rejection"));
        }
        let text = std::fs::read_to_string(dir.path().join("config-events.jsonl")).unwrap();
        let rows: Vec<Value> = text
            .lines()
            .map(|s| serde_json::from_str(s).unwrap())
            .collect();
        assert_eq!(
            rows,
            vec![
                json!({"hook_event_name":"ConfigChange","session_id":"session","cwd":dir.path(),"transcript_path":transcript(dir.path(),"session").unwrap(),"source":"user_settings","file_path":settings})
            ]
        );
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), "{}");
        crate::data_layout::write_config_file(&settings, initial).unwrap();
        check_configuration(&context, dir.path(), "session")
            .await
            .unwrap();
        context.db.create("other-session", Default::default()).unwrap();
        check_configuration(&context, dir.path(), "other-session")
            .await
            .unwrap();
    }
}
