//! Local slash commands for one-shot and stream-json input.
use crate::{
    args::{Cli, OutputFormat},
    command_catalog, repl,
    session::{emit_json, Session},
};
use anyhow::{bail, Result};
use serde_json::json;

enum Builtin<'a> {
    Compact,
    Help,
    Status,
    Context,
    Cost,
    History,
    Model,
    Unavailable(&'a str),
}

pub(crate) struct Command<'a> {
    builtin: Builtin<'a>,
    argument: &'a str,
}

impl Command<'_> {
    pub(crate) fn is_compaction(&self) -> bool {
        matches!(self.builtin, Builtin::Compact)
    }
}

pub(crate) fn parse<'a>(session: &Session, line: &'a str) -> Result<Option<Command<'a>>> {
    let line = line.trim();
    let (name, argument) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
    let Some(skill_name) = name.strip_prefix('/') else {
        return Ok(None);
    };
    let builtin = match name {
        "/compact" => Builtin::Compact,
        "/help" => Builtin::Help,
        "/status" => Builtin::Status,
        "/context" => Builtin::Context,
        "/cost" => Builtin::Cost,
        "/history" => Builtin::History,
        "/model" => Builtin::Model,
        _ if command_catalog::COMMANDS
            .iter()
            .any(|(cmd, _)| *cmd == name) =>
        {
            let context = crate::services::context(&session.paths)?;
            if crate::skill_commands::discover_commands(&context, &session.workspace)?
                .get(skill_name)
                .is_some_and(|skill| skill.user_invocable)
            {
                return Ok(None); // The ordinary turn owns skill expansion and permissions.
            }
            Builtin::Unavailable(name)
        }
        _ => return Ok(None),
    };
    Ok(Some(Command {
        builtin,
        argument: argument.trim(),
    }))
}

pub(crate) async fn run(cli: &Cli, session: &Session, cmd: Command<'_>) -> Result<()> {
    let stream = cli.output_format == OutputFormat::StreamJson;
    if stream {
        let init = tokio::select! {
            init = session.sdk_init_message() => init?,
            _ = session.sdk_cancelled() => bail!("SDK command interrupted"),
        };
        crate::sdk_messages::emit_init_once(|| init)?;
    }
    let message = match cmd.builtin {
        Builtin::Compact => manual_compact(session, cmd.argument, stream).await?,
        Builtin::Help => command_catalog::help(),
        Builtin::Status if cmd.argument.is_empty() => {
            crate::status_commands::text(session, "").await?
        }
        Builtin::Context if cmd.argument.is_empty() => repl::context_text(session)?,
        Builtin::Cost if matches!(cmd.argument, "" | "local") => {
            serde_json::to_string_pretty(&crate::display_commands::local_cost(session)?)?
        }
        Builtin::History if cmd.argument.is_empty() => {
            serde_json::to_string_pretty(&session.store.history(&session.id)?)?
        }
        Builtin::Model => match cmd.argument {
            "" => session.binding_state().model.clone(),
            "list" => serde_json::to_string_pretty(&session.sdk_models().await)?,
            _ => bail!("Model changes require the set_model control request or the --model option"),
        },
        Builtin::Unavailable(name) => bail!("{name} is not available in SDK mode"),
        _ => bail!("Unsupported arguments for a headless inspection command"),
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
