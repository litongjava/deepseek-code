//! Host-owned session hooks; engine workers consume context without rerunning hooks.
use super::{
    tool_hooks::{HookEvent, HookRunner},
    ServiceContext,
};
use anyhow::Result;
use serde_json::json;
use std::{path::Path, time::Duration};

pub(crate) fn key(session: &str) -> String {
    format!("session-start-context:{session}")
}

pub fn context(context: &ServiceContext, session: &str) -> Result<String> {
    Ok(context
        .setting::<String>(&key(session))?
        .unwrap_or_default())
}

/// A same-session worker belongs to an existing frontend; a fork owns its lifecycle.
pub fn owns_lifecycle(context: &ServiceContext, session: &str) -> Result<bool> {
    let Some(dir) = super::background_sessions::worker_directory(&context.paths.data_dir, session)?
    else {
        return Ok(true);
    };
    let job = super::background_sessions::read_job(&dir)?;
    Ok(job.parent_session_id != job.session_id)
}

pub async fn start(
    context: &ServiceContext,
    workspace: &Path,
    session: &str,
    source: &str,
    model: &str,
) -> Result<Vec<String>> {
    Ok(
        start_with_output(context, workspace, session, source, model)
            .await?
            .warnings,
    )
}

/// Activation-local output: never persisted or replayed from a resumed transcript.
/// Hosts consume the slot once; interactive hosts deliberately discard it.
pub async fn start_with_output(
    context: &ServiceContext,
    workspace: &Path,
    session: &str,
    source: &str,
    model: &str,
) -> Result<super::tool_hooks::HookOutcome> {
    anyhow::ensure!(
        ["startup", "resume", "clear", "compact"].contains(&source),
        "Invalid SessionStart source"
    );
    super::file_changed::stop(session).await;
    super::tool_hooks::agent::cancel_session(session);
    super::async_hooks::cancel_shared(&context.paths.data_dir, session).await;
    // Clear previous activation's context before running commands, including failures.
    context.save_setting(&key(session), &"")?;
    let outcome = collect_output(context, workspace, session, source, model).await?;
    context.save_setting(&key(session), &outcome.additional_context.join("\n"))?;
    Ok(outcome)
}

/// Collect context without changing persistence; compaction commits it with its boundary.
pub(crate) async fn collect(
    context: &ServiceContext,
    workspace: &Path,
    session: &str,
    source: &str,
    model: &str,
) -> Result<(String, Vec<String>)> {
    let outcome = collect_output(context, workspace, session, source, model).await?;
    Ok((outcome.additional_context.join("\n"), outcome.warnings))
}

async fn collect_output(
    context: &ServiceContext,
    workspace: &Path,
    session: &str,
    source: &str,
    model: &str,
) -> Result<super::tool_hooks::HookOutcome> {
    let outcome = HookRunner::load_with_plugins(context, workspace)?
        .run(
            HookEvent::SessionStart,
            session,
            json!({"source":source,"model":model}),
        )
        .await?;
    Ok(outcome)
}

pub fn end_timeout() -> Duration {
    Duration::from_millis(
        std::env::var("DEEPSEEK_CODE_SESSIONEND_HOOKS_TIMEOUT_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(1500),
    )
}

/// End hooks are best effort; the entire sequence shares a bounded shutdown budget.
pub async fn end(
    context: &ServiceContext,
    workspace: &Path,
    session: &str,
    reason: &str,
    budget: Duration,
) -> Vec<String> {
    super::file_changed::stop(session).await;
    super::tool_hooks::agent::cancel_session(session);
    let result = tokio::time::timeout(budget, async {
        HookRunner::load_with_plugins(context, workspace)?
            .run(HookEvent::SessionEnd, session, json!({"reason":reason}))
            .await
    })
    .await;
    super::file_changed::stop(session).await;
    super::tool_hooks::agent::cancel_session(session);
    super::async_hooks::cancel_shared(&context.paths.data_dir, session).await;
    let cleanup = super::session_environment::close(&context.paths.data_dir, session);
    let mut warnings = match result {
        Ok(Ok(outcome)) => outcome.warnings,
        Ok(Err(error)) => vec![format!("SessionEnd hook: {error:#}")],
        Err(_) => vec![format!(
            "SessionEnd hooks exceeded {} ms; shutdown continues",
            budget.as_millis()
        )],
    };
    if let Err(error) = cleanup {
        warnings.push(format!("Session environment cleanup: {error:#}"));
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn fixture() -> (tempfile::TempDir, ServiceContext) {
        let dir = tempfile::tempdir().unwrap();
        let context = ServiceContext::open(
            crate::runtime::RuntimePaths::new(dir.path().into()).unwrap(),
            Arc::new(super::super::EnvironmentCredentials),
        )
        .unwrap();
        (dir, context)
    }
    fn configure(root: &Path, hooks: serde_json::Value) {
        std::fs::write(
            root.join("settings.json"),
            json!({"hooks":hooks}).to_string(),
        )
        .unwrap();
    }
    #[tokio::test]
    async fn session_hooks_match_inputs_continue_after_failure_and_replace_context() {
        let (dir, service) = fixture();
        std::fs::write(dir.path().join("hook.cjs"), r#"const fs=require('fs'); let text=''; process.stdin.on('data',x=>text+=x); process.stdin.on('end',()=>{const v=JSON.parse(text); fs.appendFileSync('events.jsonl',JSON.stringify(v)+'\n'); if(v.hook_event_name==='SessionStart') console.log('context:'+v.source); });"#).unwrap();
        configure(
            dir.path(),
            json!({
                "SessionStart":[{"matcher":"^startup$","hooks":[{"type":"command","command":"node -e \"process.stderr.write('expected failure');process.exit(2)\""}]},{"matcher":"startup|resume","hooks":[{"type":"command","command":"node hook.cjs"}]},{"matcher":"^clear$","hooks":[{"type":"command","command":"node -e \"throw Error('wrong matcher')\""}]}],
                "SessionEnd":[{"matcher":"^other$","hooks":[{"type":"command","command":"node hook.cjs"}]}]
            }),
        );
        let warnings = start(&service, dir.path(), "session", "startup", "model")
            .await
            .unwrap();
        assert!(warnings
            .iter()
            .any(|warning| warning.contains("expected failure")));
        assert_eq!(context(&service, "session").unwrap(), "context:startup");
        assert!(start(&service, dir.path(), "session", "resume", "model")
            .await
            .unwrap()
            .is_empty());
        assert_eq!(context(&service, "session").unwrap(), "context:resume");
        assert!(end(
            &service,
            dir.path(),
            "session",
            "clear",
            Duration::from_secs(5)
        )
        .await
        .is_empty());
        assert!(end(
            &service,
            dir.path(),
            "session",
            "other",
            Duration::from_secs(5)
        )
        .await
        .is_empty());
        let events: Vec<serde_json::Value> =
            std::fs::read_to_string(dir.path().join("events.jsonl"))
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
        assert_eq!(events.len(), 3);
        assert_eq!(events[0]["session_id"], "session");
        assert_eq!(events[0]["model"], "model");
        assert_eq!(events[0]["cwd"], json!(dir.path()));
        assert_eq!(events[2]["reason"], "other");
    }
    #[tokio::test]
    async fn initial_message_is_activation_local_and_compaction_only_collects_context() {
        let (dir, service) = fixture();
        std::fs::write(dir.path().join("initial.cjs"), r#"let s='';process.stdin.on('data',x=>s+=x);process.stdin.on('end',()=>console.log(JSON.stringify({hookSpecificOutput:{hookEventName:'SessionStart',initialUserMessage:JSON.parse(s).source,additionalContext:'context'}})));"#).unwrap();
        configure(
            dir.path(),
            json!({"SessionStart":[{"hooks":[{"type":"command","command":"node initial.cjs"}]}]}),
        );
        for source in ["startup", "resume", "clear", "compact"] {
            let mut result = start_with_output(&service, dir.path(), "session", source, "model")
                .await
                .unwrap();
            assert_eq!(result.initial_user_message.take().as_deref(), Some(source));
            assert!(result.initial_user_message.take().is_none());
            assert_eq!(context(&service, "session").unwrap(), "context");
        }
        let (text, warnings) = collect(&service, dir.path(), "session", "compact", "model")
            .await
            .unwrap();
        assert_eq!(text, "context");
        assert!(warnings.is_empty());
        configure(dir.path(), json!({}));
        let fresh = start_with_output(&service, dir.path(), "session", "resume", "model")
            .await
            .unwrap();
        assert!(fresh.initial_user_message.is_none());
        assert!(context(&service, "session").unwrap().is_empty());
    }

    #[tokio::test]
    async fn session_end_budget_kills_commands_and_descendants() {
        let (dir, service) = fixture();
        std::fs::write(dir.path().join("slow.cjs"),"const fs=require('fs');require('child_process').spawn(process.execPath,['-e',\"setTimeout(()=>require('fs').writeFileSync('survived','bad'),1800)\"],{stdio:'ignore'});fs.writeFileSync('started','yes');setTimeout(()=>{},10000);").unwrap();
        configure(
            dir.path(),
            json!({"SessionEnd":[{"hooks":[{"type":"command","command":"node slow.cjs","timeout":30}]}]}),
        );
        let warnings = end(
            &service,
            dir.path(),
            "session",
            "other",
            Duration::from_millis(1000),
        )
        .await;
        assert!(dir.path().join("started").exists());
        assert!(warnings[0].contains("exceeded"));
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(!dir.path().join("survived").exists());
    }
}
