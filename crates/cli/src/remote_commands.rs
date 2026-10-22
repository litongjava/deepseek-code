//! `/remote-control` (`/rc`): exposes the interactive session to a Remote
//! Control server. Remote prompts run only while the prompt is idle.
use crate::{session::Session, terminal::input::TerminalInput};
use anyhow::{Context, Result};
use claude_core::{
    runtime::RuntimePaths,
    services::remote_control::{
        self, Bridge, Endpoint, Inbound, Registration, Status, Timing, Update,
    },
};
use serde_json::{json, Value};

const USAGE: &str = "Usage: /remote-control [NAME] [--server URL [--save]] | status | disconnect | server [URL|clear]";

#[derive(Default)]
pub struct Remote {
    bridge: Option<Bridge>,
    published: usize,
    deferred: std::collections::VecDeque<String>,
    /// Remote prompts already shown by the remote client; not echoed back.
    remote_prompts: std::collections::VecDeque<String>,
}

/// What the REPL should do with a remote update.
pub enum Action {
    None,
    Prompt(String),
    /// Session state changed; redraw the prompt.
    Refresh,
}

impl Remote {
    pub fn is_connected(&self) -> bool {
        self.bridge.is_some()
    }

    /// Runs a remote prompt once the local prompt is idle.
    pub fn defer(&mut self, prompt: String) {
        self.remote_prompts.push_back(prompt.clone());
        self.deferred.push_back(prompt);
    }

    pub fn take_deferred(&mut self) -> Option<String> {
        self.deferred.pop_front()
    }

    /// Next remote update; pending forever when not connected.
    pub async fn update(&mut self) -> Update {
        match self.bridge.as_mut() {
            Some(bridge) => bridge.update().await,
            None => std::future::pending().await,
        }
    }

    /// `notify` shows a line without disturbing an active prompt editor.
    pub async fn handle(
        &mut self,
        update: Update,
        session: &mut Session,
        notify: impl Fn(String),
    ) -> Result<Action> {
        match update {
            Update::Inbound(Inbound::Prompt(text)) => return Ok(Action::Prompt(text)),
            Update::Inbound(Inbound::Control {
                subtype,
                request,
                reply,
            }) => {
                let result = control(&subtype, &request, session).await;
                if result.is_ok() && subtype == "set_permission_mode" {
                    notify(format!(
                        "Remote Control set permission mode to {}.",
                        session.permission_mode()
                    ));
                }
                let changed = result.is_ok();
                let _ = reply.send(result.map_err(|error| format!("{error:#}")));
                if changed {
                    return Ok(Action::Refresh);
                }
            }
            Update::Status(Status::Connected) => notify("Remote Control connected.".into()),
            Update::Status(Status::Reconnecting(reason)) => {
                notify(format!("Remote Control reconnecting: {reason}"))
            }
            Update::Status(Status::Ready) => {}
            Update::Status(Status::Failed(reason)) | Update::Closed(Status::Failed(reason)) => {
                notify(format!("Remote Control failed: {reason}"));
                self.close().await;
            }
            Update::Status(Status::Disconnected) | Update::Closed(_) => {
                notify("Remote Control disconnected.".into());
                self.close().await;
            }
        }
        Ok(Action::None)
    }

    /// Publishes conversation messages added since the last call.
    pub fn sync(&mut self, session: &Session) -> Result<()> {
        let Some(bridge) = &self.bridge else {
            session.set_remote_relay(None);
            return Ok(());
        };
        let history = session.store.history(&session.id)?;
        for row in history.iter().skip(self.published) {
            let role = row["role"].as_str().unwrap_or_default();
            let content = row["content"].as_str().unwrap_or_default();
            if role == "user"
                && self
                    .remote_prompts
                    .front()
                    .is_some_and(|prompt| prompt == content)
            {
                self.remote_prompts.pop_front();
                continue;
            }
            if matches!(role, "user" | "assistant") && !content.trim().is_empty() {
                bridge.publish_message(role, content);
            }
        }
        if let Some(result) = turn_result(history.get(self.published..).unwrap_or_default()) {
            bridge.publish(result);
        }
        self.published = history.len();
        Ok(())
    }

    pub async fn command(
        &mut self,
        argument: &str,
        paths: &RuntimePaths,
        session: &Session,
        input: &mut Option<TerminalInput>,
    ) -> Result<()> {
        let words: Vec<&str> = argument.split_whitespace().collect();
        match words.as_slice() {
            ["status"] => {
                match &self.bridge {
                    Some(bridge) => eprintln!(
                        "Remote Control {}: {}",
                        describe(&bridge.status()),
                        bridge.url
                    ),
                    None => eprintln!("Remote Control is not connected."),
                }
                let settings = remote_control::load_settings(&paths.data_dir)?;
                eprintln!(
                    "Saved server: {}",
                    settings.server_url.as_deref().unwrap_or("(none)")
                );
                return Ok(());
            }
            ["disconnect"] => {
                anyhow::ensure!(self.bridge.is_some(), "Remote Control is not connected");
                return self.disconnect().await;
            }
            ["server"] => {
                let settings = remote_control::load_settings(&paths.data_dir)?;
                eprintln!(
                    "Remote Control server: {}",
                    settings.server_url.as_deref().unwrap_or("(none)")
                );
                return Ok(());
            }
            ["server", "clear"] => {
                let mut settings = remote_control::load_settings(&paths.data_dir)?;
                settings.server_url = None;
                remote_control::save_settings(&paths.data_dir, &settings)?;
                eprintln!("Remote Control server cleared.");
                return Ok(());
            }
            ["server", url] => {
                let mut settings = remote_control::load_settings(&paths.data_dir)?;
                settings.server_url = Some(
                    remote_control::validate_url(url)?
                        .as_str()
                        .trim_end_matches('/')
                        .to_owned(),
                );
                remote_control::save_settings(&paths.data_dir, &settings)?;
                eprintln!(
                    "Remote Control server saved: {}",
                    settings.server_url.unwrap_or_default()
                );
                return Ok(());
            }
            _ => {}
        }
        if let Some(bridge) = &self.bridge {
            eprintln!(
                "Remote Control {}: {}",
                describe(&bridge.status()),
                bridge.url
            );
            let answer = match input.as_mut() {
                Some(reader) => {
                    reader
                        .read_answer("Disconnect Remote Control? [y/N] ")
                        .await?
                }
                None => None,
            };
            if answer.is_some_and(|answer| matches!(answer.trim(), "y" | "Y" | "yes")) {
                return self.disconnect().await;
            }
            eprintln!("Remote Control remains connected.");
            return Ok(());
        }
        let mut server = None;
        let mut save = false;
        let mut name = Vec::new();
        let mut rest = words.iter();
        while let Some(word) = rest.next() {
            match *word {
                "--server" => server = Some(*rest.next().context(USAGE)?),
                "--save" => save = true,
                value if value.starts_with("--") => {
                    anyhow::bail!("Unknown option {value}. {USAGE}")
                }
                value => name.push(value),
            }
        }
        anyhow::ensure!(!save || server.is_some(), "--save requires --server URL");
        let endpoint = Endpoint::resolve(&paths.data_dir, server)?;
        if save {
            let mut settings = remote_control::load_settings(&paths.data_dir)?;
            settings.server_url = Some(endpoint.base_url.clone());
            remote_control::save_settings(&paths.data_dir, &settings)?;
        }
        eprintln!("Remote Control connecting to {}…", endpoint.base_url);
        let registration = registration(session, &name.join(" "))?;
        let bridge = tokio::select! {
            bridge = Bridge::connect(endpoint, registration, Timing::default()) => bridge?,
            _ = tokio::signal::ctrl_c() => anyhow::bail!("Remote Control connection cancelled"),
        };
        eprintln!(
            "Remote Control active: {}\nUse /remote-control again to see the link or disconnect.",
            bridge.url
        );
        session.set_remote_relay(Some(bridge.relay()));
        self.bridge = Some(bridge);
        // Only messages from now on are mirrored.
        self.published = session.store.history(&session.id)?.len();
        Ok(())
    }

    pub async fn disconnect(&mut self) -> Result<()> {
        let Some(bridge) = self.bridge.take() else {
            return Ok(());
        };
        self.deferred.clear();
        self.remote_prompts.clear();
        let result = bridge.disconnect().await;
        eprintln!("Remote Control disconnected.");
        result
    }

    async fn close(&mut self) {
        if let Some(bridge) = self.bridge.take() {
            let _ = bridge.disconnect().await;
        }
    }
}

fn describe(status: &Status) -> String {
    match status {
        Status::Ready => "waiting for the server".into(),
        Status::Connected => "connected".into(),
        Status::Reconnecting(reason) => format!("reconnecting ({reason})"),
        Status::Failed(reason) => format!("failed ({reason})"),
        Status::Disconnected => "disconnected".into(),
    }
}

async fn control(subtype: &str, request: &Value, session: &mut Session) -> Result<Value> {
    match subtype {
        "set_permission_mode" => {
            let mode = match request["mode"]
                .as_str()
                .context("set_permission_mode needs mode")?
            {
                "default" => "ask",
                "acceptEdits" => "accept-edits",
                "bypassPermissions" => "bypass",
                other => other,
            };
            session.set_permission_mode(mode).await?;
            Ok(json!({"mode": protocol_mode(session.permission_mode())}))
        }
        "set_model" => {
            let model = request["model"].as_str().context("set_model needs model")?;
            session.sdk_set_model(model).await?;
            Ok(json!({"model": model}))
        }
        other => anyhow::bail!("Unsupported Remote Control request: {other}"),
    }
}

fn registration(session: &Session, name: &str) -> Result<Registration> {
    let title = if name.trim().is_empty() {
        session
            .store
            .db
            .with_conn(|conn| {
                claude_core::db::conversation_repo::get_conversation(conn, &session.id)
            })??
            .and_then(|row| row.title)
            .filter(|title| !title.trim().is_empty())
            .unwrap_or_else(|| "Claude Code session".into())
    } else {
        name.trim().to_owned()
    };
    Ok(Registration {
        machine_name: std::env::var("COMPUTERNAME")
            .or_else(|_| std::env::var("HOSTNAME"))
            .unwrap_or_else(|_| "localhost".into()),
        directory: session.workspace.to_string_lossy().into_owned(),
        branch: git(session, &["branch", "--show-current"]),
        git_repo_url: git(session, &["remote", "get-url", "origin"])
            .map(|url| strip_credentials(&url)),
        title,
        model: Some(session.model.clone()),
        permission_mode: Some(protocol_mode(session.permission_mode()).to_owned()),
    })
}

/// Remote clients use SDK permission-mode names.
fn protocol_mode(mode: &str) -> &str {
    match mode {
        "ask" => "default",
        "accept-edits" => "acceptEdits",
        "bypass" => "bypassPermissions",
        other => other,
    }
}

fn git(session: &Session, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["-c", "core.fsmonitor=false"])
        .args(args)
        .current_dir(&session.workspace)
        .output()
        .ok()?;
    let value = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (output.status.success() && !value.is_empty()).then_some(value)
}

/// Removes userinfo from URL-style remotes before they leave the machine.
fn strip_credentials(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let authority = rest.split('/').next().unwrap_or_default();
            match authority.rsplit_once('@') {
                Some((_, host)) => format!("{scheme}://{host}{}", &rest[authority.len()..]),
                None => url.to_owned(),
            }
        }
        None => url.to_owned(),
    }
}

/// The `result` event for history rows added since the last sync. New rows
/// mean a turn ended, so the remote client can stop waiting. A turn that failed
/// or was interrupted before answering leaves no assistant text and is
/// reported as an error.
fn turn_result(new_rows: &[Value]) -> Option<Value> {
    let last = new_rows.last()?;
    let answered = last["role"] == "assistant"
        && last["content"].as_str().is_some_and(|text| !text.trim().is_empty());
    Some(json!({
        "type": "result",
        "subtype": if answered { "success" } else { "error_during_execution" },
        "is_error": !answered,
        "result": if answered { last["content"].clone() } else { json!("") },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_result_reports_answers_and_unanswered_turns() {
        assert!(turn_result(&[]).is_none());
        let answered = turn_result(&[
            json!({"role":"user","content":"hi"}),
            json!({"role":"assistant","content":"hello"}),
        ])
        .unwrap();
        assert_eq!(answered["subtype"], "success");
        assert_eq!(answered["is_error"], false);
        assert_eq!(answered["result"], "hello");
        for rows in [
            vec![json!({"role":"user","content":"hi"})],
            vec![json!({"role":"user","content":"hi"}), json!({"role":"assistant","content":""})],
        ] {
            let failed = turn_result(&rows).unwrap();
            assert_eq!(failed["subtype"], "error_during_execution");
            assert_eq!(failed["is_error"], true);
        }
    }

    #[test]
    fn remote_urls_never_carry_credentials() {
        assert_eq!(
            strip_credentials("https://user:secret@github.com/o/r.git"),
            "https://github.com/o/r.git"
        );
        assert_eq!(
            strip_credentials("git@github.com:o/r.git"),
            "git@github.com:o/r.git"
        );
    }

    #[test]
    fn permission_modes_use_protocol_names() {
        assert_eq!(protocol_mode("ask"), "default");
        assert_eq!(protocol_mode("accept-edits"), "acceptEdits");
        assert_eq!(protocol_mode("bypass"), "bypassPermissions");
        assert_eq!(protocol_mode("plan"), "plan");
    }
}
