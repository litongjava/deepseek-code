//! Local slash commands for one-shot and stream-json input.
use crate::{
    args::{Cli, OutputFormat},
    command_catalog, repl,
    session::{emit_json, Session},
};
use anyhow::{bail, Result};
use serde_json::json;

pub(crate) struct Command<'a> {
    line: &'a str,
    name: &'a str,
    argument: &'a str,
}

impl Command<'_> {
    pub(crate) fn is_compaction(&self) -> bool {
        self.name == "/compact"
    }
}

// Skills have their own invocation and permission handling inside Session::turn.
pub(crate) fn parse(line: &str) -> Option<Command<'_>> {
    let line = line.trim();
    let (name, argument) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    command_catalog::COMMANDS
        .iter()
        .any(|(cmd, _)| *cmd == name)
        .then_some(Command {
            line,
            name,
            argument: argument.trim(),
        })
}

pub(crate) async fn run(cli: &Cli, session: &mut Session, cmd: Command<'_>) -> Result<()> {
    let stream = cli.output_format == OutputFormat::StreamJson;
    if stream {
        let init = tokio::select! {
            init = session.sdk_init_message() => init?,
            _ = session.sdk_cancelled() => bail!("SDK command interrupted"),
        };
        crate::sdk_messages::emit_init_once(|| init)?;
    }
    let paths = session.paths.clone();
    let message = match cmd.name {
        "/compact" => manual_compact(session, cmd.argument, stream).await?,
        "/help" => command_catalog::help(),
        "/status" => crate::status_commands::text(session, cmd.argument).await?,
        "/cd" | "/worktree"
            if session.sdk_permissions.is_some()
                && !cmd.argument.is_empty()
                && !(cmd.name == "/worktree" && cmd.argument == "status") =>
        {
            bail!(
                "{} changes the workspace. Start an SDK stream in the destination workspace.",
                cmd.name
            )
        }
        "/clear" | "/new" | "/reset" | "/branch" | "/resume"
            if session.sdk_permissions.is_some()
                && !(cmd.name == "/resume" && cmd.argument.is_empty()) =>
        {
            bail!(
                "{} switches sessions. Start a new SDK stream for the destination session.",
                cmd.name
            )
        }
        "/model" => {
            match cmd.argument {
                "" => {}
                "list" => {
                    return emit_result(
                        cli,
                        session,
                        &serde_json::to_string_pretty(&session.sdk_models().await)?,
                    )
                }
                name => session.sdk_set_model(name).await?,
            }
            session.binding_state().model.clone()
        }
        "/history" => serde_json::to_string_pretty(&session.store.history(&session.id)?)?,
        _ => {
            use crate::terminal_output::COMMAND_OUTPUT;
            COMMAND_OUTPUT
                .scope(Default::default(), async {
                    if !repl::handle(cmd.line, &paths, session, None).await?
                        && !crate::design_commands::handle(
                            cli,
                            cmd.name,
                            cmd.argument,
                            &paths,
                            session,
                        )
                        .await?
                        && !crate::batch_commands::handle(
                            cli,
                            cmd.name,
                            cmd.argument,
                            &paths,
                            session,
                        )
                        .await?
                    {
                        bail!("{} requires an interactive session", cmd.name);
                    }
                    Ok::<_, anyhow::Error>(
                        COMMAND_OUTPUT.with(|out| out.take().trim_end().to_owned()),
                    )
                })
                .await?
        }
    };
    emit_result(cli, session, &message)
}

fn emit_result(cli: &Cli, session: &Session, message: &str) -> Result<()> {
    if cli.output_format == OutputFormat::Text {
        println!("{message}");
    } else {
        emit_json(
            &json!({"type":"result","subtype":"success","is_error":false,
            "session_id":session.id,"result":message,"num_turns":0,"permission_denials":[],
            "uuid":uuid::Uuid::new_v4().to_string()}),
        )?;
    }
    Ok(())
}

async fn manual_compact(session: &Session, focus: &str, stream: bool) -> Result<String> {
    let _lease = session.selected_session()?.acquire_execution()?;
    let mut progress = deepseek_code_core::services::hook_progress::subscribe(&session.id);
    let future = repl::compact(&session.paths, session, focus, None);
    tokio::pin!(future);
    let compacted = loop {
        tokio::select! {
            result = &mut future => break result,
            active = progress.changed(), if stream => {
                let messages: Vec<_> = active.into_iter().map(|(_, text)| text).collect();
                if !messages.is_empty() {
                    emit_json(&json!({"type":"compaction_notice","message":messages.join(" · ")}))?;
                }
            }
        }
    };
    let compacted = compacted.inspect_err(|error| {
        if stream {
            let _ = emit_json(&json!({"type":"compaction_notice","message":format!("{error:#}")}));
        }
    })?;
    let report = compacted.report;
    let mut message = crate::compaction::completion_report(&report);
    if compacted.cancelled_after_commit {
        message.push_str(
            "\nStopped the remaining post-compaction steps; the summary was already saved.",
        );
    }
    if stream {
        emit_json(&json!({"type":"context_reset","reason":"manual_compact",
            "session_id":session.id,"boundary_id":report.message_id}))?;
        for notice in report.warnings.iter().chain(std::iter::once(&message)) {
            emit_json(&json!({"type":"compaction_notice","message":notice}))?;
        }
    }
    Ok(message)
}
