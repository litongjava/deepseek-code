//! Explicit per-session tool rules; plan mode remains read-only and deny wins.
use crate::session::Session;
use anyhow::{Context, Result};
use crate::terminal::tool_view::Remember;
use claude_core::{db::DbManager, native_engine::permission_response::Commit, permissions::rules::PermissionAction};
use std::collections::HashMap;

/// Saves the session's rules and applies them at once.
pub fn save_rules(session: &Session, rules: &HashMap<String, PermissionAction>) -> Result<()> {
    save(&session.store.db, &session.id, rules)?;
    session.permissions.set_session_rules(&session.id, rules.clone());
    Ok(())
}

pub fn load(session: &Session) -> Result<HashMap<String, PermissionAction>> {
    load_rules(&session.store.db, &session.id)
}

fn load_rules(db: &DbManager, session: &str) -> Result<HashMap<String, PermissionAction>> {
    db.with_conn(|conn| -> Result<_> {
        let mut statement = conn.prepare("SELECT value FROM peripheral_settings WHERE key=?1")?;
        let mut rows = statement.query([format!("cli.permissions.{session}")])?;
        Ok(match rows.next()? {
            Some(row) => serde_json::from_str(&row.get::<_, String>(0)?)?,
            None => HashMap::new(),
        })
    })?
}

fn save(db: &DbManager, session: &str, rules: &HashMap<String, PermissionAction>) -> Result<()> {
    let value = serde_json::to_string(rules)?;
    db.with_conn(|conn| conn.execute("INSERT INTO peripheral_settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [format!("cli.permissions.{session}"), value]))??;
    Ok(())
}

/// The approval prompt's session choice, published by the engine only after
/// it accepts the reviewed call: an allow rule saved for this session,
/// accept-edits mode or auto mode.
pub fn remember_commit(session: &Session, remember: &Remember) -> Result<Commit> {
    let (db, id, manager) = (session.store.db.clone(), session.id.clone(), session.permissions.clone());
    Ok(match remember.clone() {
        Remember::AcceptEdits | Remember::Auto => {
            let name = if *remember == Remember::Auto { "auto" } else { "accept-edits" };
            let mode = claude_core::services::session_permission_mode::parse(name)?;
            Box::new(move || claude_core::services::session_permission_mode::set(&db, &id, &manager, mode))
        }
        Remember::Rule(rule) => {
            claude_core::permissions::patterns::validate(&rule)?;
            Box::new(move || {
                let mut rules = load_rules(&db, &id)?;
                rules.insert(rule, PermissionAction::Allow);
                save(&db, &id, &rules)?;
                manager.set_session_rules(&id, rules);
                Ok(())
            })
        }
    })
}

/// Saves a remembered allow rule to the project's `.claude/settings.local.json`,
/// as the reference CLI does, so later sessions keep it.
pub fn save_local(session: &Session, rule: &str) -> Result<()> {
    claude_core::services::permission_updates::add_local_allow_rule(&session.workspace, &settings_rule(rule))?;
    Ok(())
}

/// The rule as the reference CLI saves it: a sandbox host becomes a
/// `WebFetch(domain:)` rule, which the sandbox allowlist also reads.
fn settings_rule(rule: &str) -> String {
    match rule.strip_prefix("SandboxNetwork(") {
        Some(rest) => format!("WebFetch({rest}"),
        None => rule.to_owned(),
    }
}

fn words(argument: &str) -> Vec<&str> {
    let argument = argument.trim();
    let (first, rest) = argument
        .split_once(char::is_whitespace)
        .map(|(a, b)| (a, b.trim()))
        .unwrap_or((argument, ""));
    if first.is_empty() {
        vec![]
    } else if rest.is_empty() {
        vec![first]
    } else {
        vec![first, rest]
    }
}

pub async fn handle(argument: &str, session: &mut Session) -> Result<()> {
    if argument.trim().is_empty() && crate::terminal::picker::available() {
        // As in the reference CLI: tabs of rules instead of a printed list.
        return crate::permissions_menu::run(session).await;
    }
    if let Some(action) = argument.trim().strip_prefix("auto-config").filter(|rest| rest.is_empty() || rest.starts_with(char::is_whitespace)) {
        let context = crate::services::context(&session.paths)?;
        let action = action.trim();
        let mut config = claude_core::permissions::auto_config::load(&context)?;
        match action {
            "" | "show" => {},
            "off" => {
                config.enabled = false;
                claude_core::permissions::auto_config::save(&context, &config)?;
                if session.permissions.get_mode() == claude_core::permissions::PermissionMode::Auto { session.set_permission_mode("ask").await?; }
            },
            value if value.starts_with("set ") => {
                config = serde_json::from_str(value[4..].trim())?;
                claude_core::permissions::auto_config::save(&context, &config)?;
            },
            _ => anyhow::bail!("Usage: /permissions auto-config [show|set JSON|off]"),
        }
        return crate::services::print(&config);
    }
    let words = words(argument);
    let mut rules = load(session)?;
    match words.as_slice() {
        [] | ["list"] => {},
        [mode] if claude_core::permissions::PermissionMode::parse(mode).is_ok() => session.set_permission_mode(mode).await?,
        [action @ ("allow" | "deny" | "ask" | "remove"), tool] => {
            claude_core::permissions::patterns::validate(tool)?;
            match *action {
                "remove" => { rules.remove(*tool); },
                "allow" => { rules.insert((*tool).into(), PermissionAction::Allow); },
                "deny" => { rules.insert((*tool).into(), PermissionAction::Deny); },
                "ask" => { rules.insert((*tool).into(), PermissionAction::Confirm); },
                _ => unreachable!(),
            }
            save(&session.store.db, &session.id, &rules)?;
        }
        _ => anyhow::bail!("Usage: /permissions [list|ask|accept-edits|plan|bypass|dont-ask|auto|auto-config show|auto-config set JSON|auto-config off|allow RULE|deny RULE|ask RULE|remove RULE]. Examples: allow Bash(git status *), deny Read(.env), ask Edit(src/**). Enter rules without outer shell quotes."),
    }
    session
        .permissions
        .set_session_rules(&session.id, rules.clone());
    eprintln!("{}", launch_rule_display(session).await?);
    if rules.is_empty() {
        eprintln!("  (none)");
    }
    for (tool, action) in rules
        .into_iter()
        .collect::<std::collections::BTreeMap<_, _>>()
    {
        eprintln!("  {tool}: {action:?}");
    }
    Ok(())
}

async fn launch_rule_display(session: &Session) -> Result<String> {
    use claude_core::services::{background_sessions as jobs, session_worker};
    use std::fmt::Write;
    let active = jobs::list(&session.paths.data_dir, None)?
        .into_iter()
        .find(|job| {
            job.session_id == session.id
                && job.finished_ms.is_none()
                && matches!(job.status.as_str(), "starting" | "running" | "unresponsive")
        });
    let (mut display, launch) = if let Some(job) = active {
        let reply =
            session_worker::permission_snapshot(&session.paths.data_dir, &session.id, &job.id)
                .await
                .context("Active worker permissions unavailable")?;
        active_launch_snapshot(reply, &job.id)?
    } else {
        (
            format!("Permission mode: {}\n", session.permission_mode()),
            session.permissions.launch_rules(&session.id),
        )
    };
    display.push_str("Launch rules (command-line options):\n");
    if !launch.is_empty() {
        for (pattern, action) in launch
            .into_iter()
            .collect::<std::collections::BTreeMap<_, _>>()
        {
            writeln!(display, "  {pattern}: {action:?}")?;
        }
    } else {
        display.push_str("  (none)\n");
    }
    write!(display, "Saved session rules ({}):", session.id)?;
    Ok(display)
}

fn active_launch_snapshot(
    reply: claude_core::services::session_worker::Reply,
    job_id: &str,
) -> Result<(String, HashMap<String, PermissionAction>)> {
    anyhow::ensure!(
        reply.ok && reply.job_id.as_deref() == Some(job_id),
        "Permission response targets a different or failed job"
    );
    let target = reply
        .permission_session_id
        .context("Worker did not identify its permission session")?;
    let mode = reply
        .permission_mode
        .context("Worker did not return its permission mode")?;
    let rules = reply.launch_rules.context(
        "Worker does not expose launch rules; restart it to inspect its active permissions",
    )?;
    Ok((
        format!(
            "Active worker job: {job_id}\nPermission session: {target}\nPermission mode: {mode}\n"
        ),
        rules,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_hosts_are_saved_as_web_fetch_domains() {
        assert_eq!(settings_rule("SandboxNetwork(domain:pypi.org)"), "WebFetch(domain:pypi.org)");
        assert_eq!(settings_rule("Bash(npm run *)"), "Bash(npm run *)");
    }

    #[test]
    fn active_worker_display_requires_authoritative_launch_rules_and_job_identity() {
        use claude_core::services::session_worker::Reply;
        let snapshot = || {
            let mut reply = Reply::ok(Some("active-job".into()));
            reply.permission_session_id = Some("owned-child".into());
            reply.permission_mode = Some("bypass".into());
            reply.launch_rules = Some(HashMap::from([("Write".into(), PermissionAction::Deny)]));
            reply
        };
        let (display, rules) = active_launch_snapshot(snapshot(), "active-job").unwrap();
        assert!(display.contains("Permission session: owned-child"));
        assert!(display.contains("Permission mode: bypass"));
        assert_eq!(rules.get("Write"), Some(&PermissionAction::Deny));
        assert!(active_launch_snapshot(snapshot(), "another-job").is_err());
        let mut old_worker = snapshot();
        old_worker.launch_rules = None;
        assert!(active_launch_snapshot(old_worker, "active-job")
            .unwrap_err()
            .to_string()
            .contains("does not expose launch rules"));
    }

    #[test]
    fn permission_command_preserves_spaces_in_rule_and_serialization() {
        assert_eq!(
            words(" allow Bash(git status *) "),
            vec!["allow", "Bash(git status *)"]
        );
        assert_eq!(
            words("deny Read(my files/**)"),
            vec!["deny", "Read(my files/**)"]
        );
        let rules = HashMap::from([
            ("Bash(git status *)".to_string(), PermissionAction::Allow),
            ("Read(my files/**)".to_string(), PermissionAction::Deny),
        ]);
        let restored: HashMap<String, PermissionAction> =
            serde_json::from_str(&serde_json::to_string(&rules).unwrap()).unwrap();
        assert_eq!(rules, restored);
        for rule in restored.keys() {
            claude_core::permissions::patterns::validate(rule).unwrap();
        }
    }
}
