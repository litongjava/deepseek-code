//! Explicit plan/run orchestration for independent worktree tasks.
use crate::{args::Cli, session::Session};
use anyhow::{ensure, Context, Result};
use deepseek_code_core::{
    runtime::RuntimePaths,
    services::{
        background_sessions,
        batch::{self, Batch, WorkerLaunch},
    },
};
use std::{process::Stdio, time::Duration};

/// The planner runs as an ordinary read-only session, so it must not inherit
/// the parent worker's control channel, job or model snapshot. Only the
/// credential forwarded by `launch` is applied afterwards.
fn planner_command(
    executable: &std::path::Path,
    arguments: &[String],
    options: &[&str],
    prompt: &str,
) -> tokio::process::Command {
    let mut process = tokio::process::Command::new(executable);
    deepseek_code_core::services::background_sessions::strip_internal_env(&mut process);
    process
        .args(arguments)
        .args(options)
        .args(background_sessions::PromptOrigin::Generated.arguments(prompt));
    process
}

fn display(batch: &Batch) {
    command_eprintln!(
        "Batch {} — {}\n{}\nBase commit: {}\nVerification: {}",
        batch.id,
        batch.status,
        batch.proposal.summary,
        batch.base_commit,
        batch.proposal.verification
    );
    for (index, worker) in batch.workers.iter().enumerate() {
        command_eprintln!(
            "{}. {} [{}]\n   paths: {}\n   branch: {}",
            index + 1,
            worker.title,
            worker.status,
            batch.proposal.units[index].paths.join(", "),
            worker.branch
        );
        command_eprintln!("   task: {}", batch.proposal.units[index].instructions);
        if let Some(job) = &worker.job_id {
            command_eprintln!("   task ID: {job}");
        }
        if let Some(pr) = &worker.pr {
            command_eprintln!("   verified PR: {pr}");
        }
        if let Some(note) = &worker.note {
            command_eprintln!("   {note}");
        }
    }
    if let Some(error) = &batch.error {
        command_eprintln!("Batch error: {error}");
    }
    if batch.status == "planned" {
        command_eprintln!("No workers started. /batch run {} starts isolated workers using the current permission mode and existing deny rules. Answer worker permission requests using /tasks pending and /tasks answer. Add --permission-mode MODE only to explicitly choose a different mode. Local commits only; add --publish to authorize pushes and PR creation.",batch.id);
    } else {
        command_eprintln!(
            "Concurrency: {}; permissions: {}; publication: {}",
            batch.concurrency,
            batch.permission_mode,
            if batch.publish {
                "authorized"
            } else {
                "disabled"
            }
        );
    }
}

async fn launch(
    cli: &Cli,
    paths: &RuntimePaths,
    session: &Session,
) -> Result<(WorkerLaunch, Vec<(String, String)>)> {
    let provider = session.launch_provider().await?;
    let mut arguments = vec![
        "--data-dir".into(),
        paths.data_dir.to_string_lossy().into_owned(),
        "--provider".into(),
        session.binding_state().provider_id.clone(),
        "--model".into(),
        session.binding_state().model.clone(),
    ];
    arguments.extend(provider.provider.child_transport_arguments());
    let mut environment = Vec::new();
    if !provider.provider.api_key.is_empty() {
        arguments.extend(["--api-key-env".into(), "CLAUDE_BATCH_API_KEY".into()]);
        environment.push((
            "CLAUDE_BATCH_API_KEY".into(),
            provider.provider.api_key.clone(),
        ));
    }
    if let Some(effort) = crate::session_commands::effective_effort(session, session.admitted_launch_options(cli).and_then(|args| args.effort.as_deref()).or(provider.model.effort.as_deref()))?
    {
        arguments.extend(["--effort".into(), effort]);
    }
    if let Some(limit) = session.admitted_launch_options(cli).and_then(|args| args.max_tokens).or(provider.model.max_tokens) {
        arguments.extend(["--max-tokens".into(), limit.to_string()]);
    }
    for rule in &cli.disallowed_tools {
        arguments.push(format!("--disallowed-tools={rule}"));
    }
    if let Some(turns) = cli.max_turns {
        arguments.extend(["--max-turns".into(), turns.to_string()]);
    }
    let system = cli.system_prompt_text()?;
    if !system.is_empty() {
        arguments.extend(["--system-prompt".into(), system]);
    }
    Ok((
        WorkerLaunch {
            executable: std::env::current_exe()?,
            arguments,
            model: session.binding_state().model.clone(),
            provider: session.binding_state().provider_id.clone(),
            permission_mode: session.permission_mode().into(),
        },
        environment,
    ))
}

pub async fn handle(
    cli: &Cli,
    command: &str,
    argument: &str,
    paths: &RuntimePaths,
    session: &mut Session,
) -> Result<bool> {
    if command != "/batch" {
        return Ok(false);
    }
    let parent = session.selected_session()?;
    let _guard = parent.hold_incarnation()?;
    let argument = argument.trim();
    let mut words = argument.split_whitespace();
    match words.next() {
        None | Some("list") => {
            for batch in batch::list(&paths.data_dir, &parent)? {
                display(&batch);
            }
            if argument.is_empty() {
                command_eprintln!("Usage: /batch INSTRUCTION | /batch show ID | /batch run ID [--concurrency N] [--permission-mode MODE] [--publish] | /batch cancel ID | /batch logs ID UNIT");
            }
        }
        Some("show" | "cancel" | "logs") => {
            let action = argument.split_whitespace().next().unwrap();
            let id = words.next().context("A batch ID is required")?;
            let state = batch::get(&paths.data_dir, id, &parent)?;
            ensure!(
                state.parent_session_id == session.id,
                "Resume the batch's originating session to manage it"
            );
            match action {
                "cancel" => {
                    ensure!(words.next().is_none(), "Usage: /batch cancel ID");
                    batch::cancel(&paths.data_dir, id, &parent)?;
                    command_eprintln!("Cancellation requested; inspect /batch show {id} for confirmed completion.");
                }
                "logs" => {
                    let index = words
                        .next()
                        .context("Usage: /batch logs ID UNIT_NUMBER")?
                        .parse::<usize>()?;
                    ensure!(
                        words.next().is_none() && index > 0,
                        "Usage: /batch logs ID UNIT_NUMBER"
                    );
                    let worker = state
                        .workers
                        .get(index - 1)
                        .context("Unknown unit number")?;
                    let job = worker
                        .job_id
                        .as_deref()
                        .context("This worker has not started")?;
                    let record = background_sessions::get(&paths.data_dir, job)?;
                    let _participants = background_sessions::hold_owned_attachment(
                        &paths.data_dir,
                        &record,
                        &session.selected_session()?,
                    )?;
                    let (output, error) = background_sessions::output(&paths.data_dir, job)?;
                    command_println!("{output}");
                    command_eprintln!("{error}");
                }
                _ => {
                    ensure!(words.next().is_none(), "Usage: /batch show ID");
                    display(&state);
                }
            }
        }
        Some("run") => {
            let id = words.next().context(
                "Usage: /batch run ID [--concurrency N] [--permission-mode MODE] [--publish]",
            )?;
            let state = batch::get(&paths.data_dir, id, &parent)?;
            ensure!(
                state.parent_session_id == session.id,
                "Resume the batch's originating session before running it"
            );
            let mut concurrency = 4;
            let mut publish = false;
            let mut permission_mode = session.permission_mode().to_string();
            while let Some(option) = words.next() {
                match option {
                    "--concurrency" => {
                        concurrency = words
                            .next()
                            .context("Missing concurrency")?
                            .parse::<usize>()?
                    }
                    "--publish" => publish = true,
                    "--permission-mode" => {
                        permission_mode = words.next().context("Missing permission mode")?.into()
                    }
                    _ => anyhow::bail!("Unknown batch run option: {option}"),
                }
            }
            let (mut launch, environment) = launch(cli, paths, session).await?;
            launch.permission_mode = permission_mode;
            let batch = batch::start(
                &paths.data_dir,
                id,
                &parent,
                launch,
                concurrency,
                publish,
                &environment,
            )?;
            display(&batch);
        }
        _ => {
            let instruction = argument.strip_prefix("plan ").unwrap_or(argument).trim();
            ensure!(!instruction.is_empty(), "Provide a batch instruction");
            let (workspace, head) = batch::repository(&session.workspace)?;
            let (launch, environment) = launch(cli, paths, session).await?;
            let planner_id =
                session
                    .store
                    .create(&workspace, &session.binding_state().model, &session.binding_state().provider_id)?;
            crate::session_commands::copy_settings(&session.store.db, &session.id, &planner_id)?;
            if let Some(agent) =
                deepseek_code_core::services::agents::for_session(&session.store.db, &session.id)?
            {
                deepseek_code_core::services::agents::bind(&session.store.db, &planner_id, &agent)?;
            }
            let prompt=format!("Research this repository using read-only tools and plan the user's batch change: {instruction}\nReturn 5–30 roughly equal independent units, each mergeable without other units. Give each unit non-overlapping repository-relative file/directory paths (no traversal), a precise title and self-contained implementation instructions. Include observed code conventions in summary, and a concrete shared unit/e2e verification recipe. Do not fabricate repository findings. Do not modify files or run workers. Return only the requested structured plan.");
            let mut process = planner_command(
                &launch.executable,
                &launch.arguments,
                &[
                    "--path",
                    workspace.to_str().context("Non-UTF8 workspace")?,
                    "--resume",
                    &planner_id,
                    "--permission-mode",
                    "plan",
                    "--print",
                    "--output-format",
                    "json",
                    "--json-schema",
                    &batch::schema().to_string(),
                ],
                &prompt,
            );
            process
                .envs(environment)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            #[cfg(windows)]
            process.creation_flags(0x08000000);
            command_eprintln!("Researching a batch plan in a separate read-only session. No workers will start until /batch run ID.");
            let output = tokio::select! {result=tokio::time::timeout(Duration::from_secs(600),process.output())=>result.context("Batch planning timed out")??,_=crate::signals::interrupted()=>anyhow::bail!("Batch planning cancelled")};
            ensure!(
                output.status.success(),
                "Batch planner failed: {}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            let result: serde_json::Value = serde_json::from_slice(&output.stdout)
                .context("Batch planner did not return a JSON result")?;
            let proposal: batch::Proposal =
                serde_json::from_value(result["structured_output"].clone())
                    .context("Batch planner returned an invalid plan")?;
            ensure!(
                batch::repository(&workspace)?.1 == head,
                "Repository changed while planning; generate a fresh plan"
            );
            display(&batch::create(
                &paths.data_dir,
                &parent,
                &workspace,
                instruction,
                proposal,
            )?);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn planner_drops_inherited_worker_state() {
        let command = planner_command(std::path::Path::new("fixture"), &[], &["--print"], "--plan");
        assert_eq!(
            command.as_std().get_args().collect::<Vec<_>>(),
            ["--print", "--prompt-origin=generated", "--", "--plan"]
        );
        let stripped: Vec<_> = command.as_std().get_envs().collect();
        for name in deepseek_code_core::services::background_sessions::INTERNAL_ENV {
            assert!(
                stripped
                    .iter()
                    .any(|(entry, value)| *entry == name && value.is_none()),
                "{name} was not stripped"
            );
        }
    }
}
