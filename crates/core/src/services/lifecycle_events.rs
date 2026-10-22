//! Lifecycle dispatch shared by standalone hosts and the native engine.
use super::{
    tool_hooks::{HookEvent, HookOutcome, HookRunner},
    ServiceContext,
};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::{
    io::Write,
    path::{Path, PathBuf},
};

/// Resolve the advertised transcript location without accessing storage.
pub(crate) fn transcript_path(root: &Path, session: &str) -> PathBuf {
    let name = session
        .as_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    root.join("hook-transcripts").join(format!("{name}.jsonl"))
}

/// A real, readable JSONL snapshot of the durable conversation (no request headers).
pub(crate) fn transcript(root: &Path, session: &str) -> Result<PathBuf> {
    let path = transcript_path(root, session);
    let directory = root.join("hook-transcripts");
    std::fs::create_dir_all(&directory)?;
    let mut snapshot = tempfile::NamedTempFile::new_in(&directory)?;
    let database = root.join(crate::runtime::DATABASE_FILE);
    if database.exists() {
        let conn = rusqlite::Connection::open_with_flags(
            database,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        for row in crate::db::message_repo::get_messages_by_conversation(&conn, session)? {
            writeln!(
                snapshot,
                "{}",
                json!({"type":row.role,"uuid":row.id,"sessionId":session,"timestamp":row.created_at,"message":{"role":row.role,"content":row.content}})
            )?;
        }
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
    context.save_setting(&super::session_hooks::key(session), &text)?;
    Ok(result.warnings)
}

/// Check before consuming changed settings. Keep the accepted hook definitions so a
/// changed file cannot remove its own ConfigChange guard. A denied change fails closed
/// until corrected; the original settings file is never rewritten.
pub async fn check_configuration(
    context: &ServiceContext,
    workspace: &Path,
    session: &str,
) -> Result<()> {
    let sources = [
        (
            "user_settings",
            context.paths.data_dir.join("settings.json"),
        ),
        ("project_settings", workspace.join(".claude/settings.json")),
        (
            "local_settings",
            workspace.join(".claude/settings.local.json"),
        ),
    ];
    let key = format!("lifecycle-config:{session}:{}", workspace.display());
    let mut current = Vec::new();
    for (_, path) in &sources {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e).with_context(|| format!("Reading {}", path.display())),
        };
        current.push(text);
    }
    let rejected_key = format!("{key}:rejected");
    if let Some((rejected, reason)) =
        context.setting::<(Vec<Option<String>>, String)>(&rejected_key)?
    {
        if rejected == current {
            anyhow::bail!("{reason}");
        }
    }
    if let Some(previous) = context.setting::<Vec<Option<String>>>(&key)? {
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
                    context.save_setting(&rejected_key, &(current, &reason))?;
                    anyhow::bail!("{reason}");
                }
            }
        } else {
            context.save_setting(
                &rejected_key,
                &(Vec::<Option<String>>::new(), String::new()),
            )?;
            return Ok(());
        }
    }
    // Validate before accepting, including event/type restrictions.
    HookRunner::load_with_plugins(context, workspace)?;
    context.save_setting(
        &rejected_key,
        &(Vec::<Option<String>>::new(), String::new()),
    )?;
    context.save_setting(&key, &current)
}

pub async fn instructions(
    context: &ServiceContext,
    runner: &HookRunner,
    session: &str,
    files: &[(PathBuf, String)],
) -> Result<()> {
    let key = format!("lifecycle-instructions:{session}");
    let mut seen = context
        .setting::<Vec<(PathBuf, String)>>(&key)?
        .unwrap_or_default();
    let boundary_key = format!("{key}:boundary");
    let boundary = context.db.with_conn(|conn| {
        use rusqlite::OptionalExtension;
        conn.query_row("SELECT id FROM messages WHERE conversation_id=?1 AND is_compact_boundary=1 ORDER BY sort_order DESC LIMIT 1", [session], |row| row.get::<_, String>(0)).optional()
    })??;
    let previous_boundary = context.setting::<Option<String>>(&boundary_key)?.flatten();
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
        context.save_setting(&key, &seen)?;
    }
    context.save_setting(&key, &seen)?;
    context.save_setting(&boundary_key, &boundary)?;
    Ok(())
}

pub fn api_error_kind(error: &str) -> &'static str {
    // Classify the transport status, never numbers supplied by the server body.
    let status = error
        .split_once("Model API error ")
        .and_then(|(_, tail)| tail.split(':').next())
        .and_then(|status| status.trim().parse::<u16>().ok());
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
        std::fs::write(dir.path().join("settings.json"), json!({"hooks":{"InstructionsLoaded":[{"hooks":[{"type":"command","command":format!("node \"{}\"",script.display())}]}]}}).to_string()).unwrap();
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
            .with_conn(|conn| {
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
            .unwrap()
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
        let script = dir.path().join("guard.cjs");
        std::fs::write(&script, r#"const fs=require('fs');let s='';process.stdin.on('data',x=>s+=x);process.stdin.on('end',()=>{fs.appendFileSync('config-events.jsonl',JSON.stringify(JSON.parse(s))+'\n');console.log(JSON.stringify({decision:'block',reason:'fixture rejection'}))});"#).unwrap();
        let settings = dir.path().join("settings.json");
        let initial = json!({"hooks":{"ConfigChange":[{"matcher":"^user_settings$","hooks":[{"type":"command","command":format!("node \"{}\"",script.display())}]}]}}).to_string();
        std::fs::write(&settings, &initial).unwrap();
        check_configuration(&context, dir.path(), "session")
            .await
            .unwrap();
        assert!(!dir.path().join("config-events.jsonl").exists());
        std::fs::write(&settings, "{}").unwrap();
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
        std::fs::write(settings, initial).unwrap();
        check_configuration(&context, dir.path(), "session")
            .await
            .unwrap();
        check_configuration(&context, dir.path(), "other-session")
            .await
            .unwrap();
    }
}
