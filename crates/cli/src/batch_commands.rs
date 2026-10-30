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

fn display(batch: &Batch) {
    eprintln!(
        "Batch {} — {}\n{}\nBase commit: {}\nVerification: {}",
        batch.id,
        batch.status,
        batch.proposal.summary,
        batch.base_commit,
        batch.proposal.verification
    );
    for (index, worker) in batch.workers.iter().enumerate() {
        eprintln!(
            "{}. {} [{}]\n   paths: {}\n   branch: {}",
            index + 1,
            worker.title,
            worker.status,
            batch.proposal.units[index].paths.join(", "),
            worker.branch
        );
        eprintln!("   task: {}", batch.proposal.units[index].instructions);
        if let Some(job) = &worker.job_id {
            eprintln!("   task ID: {job}");
        }
        if let Some(pr) = &worker.pr {
            eprintln!("   verified PR: {pr}");
        }
        if let Some(note) = &worker.note {
            eprintln!("   {note}");
        }
    }
    if let Some(error) = &batch.error {
        eprintln!("Batch error: {error}");
    }
    if batch.status == "planned" {
        eprintln!("No workers started. /batch run {} starts isolated workers using the current permission mode and existing deny rules. Answer worker permission requests using /tasks pending and /tasks answer. Add --permission-mode MODE only to explicitly choose a different mode. Local commits only; add --publish to authorize pushes and PR creation.",batch.id);
    } else {
        eprintln!(
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
    let provider = session.selected_provider().await?;
    let mut arguments = vec![
        "--data-dir".into(),
        paths.data_dir.to_string_lossy().into_owned(),
        "--provider".into(),
        session.provider_id.clone(),
        "--model".into(),
        session.model.clone(),
    ];
    if provider.provider.builtin_id.is_none() {
        arguments.extend([
            "--base-url".into(),
            provider.provider.base_url.clone(),
            "--api-format".into(),
            match provider.provider.api_format {
                deepseek_code_core::native_engine::provider_manager::ApiFormat::Anthropic => "anthropic",
                deepseek_code_core::native_engine::provider_manager::ApiFormat::OpenAI => "openai",
            }
            .into(),
        ]);
    }
    let mut environment = Vec::new();
    if !provider.provider.api_key.is_empty() {
        arguments.extend(["--api-key-env".into(), "CLAUDE_BATCH_API_KEY".into()]);
        environment.push((
            "CLAUDE_BATCH_API_KEY".into(),
            provider.provider.api_key.clone(),
        ));
    }
    if let Some(effort) = provider.model.effort.as_ref().or(cli.effort.as_ref()) {
        arguments.extend(["--effort".into(), effort.clone()]);
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
            model: session.model.clone(),
            provider: session.provider_id.clone(),
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
    let argument = argument.trim();
    let mut words = argument.split_whitespace();
    match words.next() {
        None | Some("list") => {
            for batch in batch::list(&paths.data_dir, &session.id)? {
                display(&batch);
            }
            if argument.is_empty() {
                eprintln!("Usage: /batch INSTRUCTION | /batch show ID | /batch run ID [--concurrency N] [--permission-mode MODE] [--publish] | /batch cancel ID | /batch logs ID UNIT");
            }
        }
        Some("show" | "cancel" | "logs") => {
            let action = argument.split_whitespace().next().unwrap();
            let id = words.next().context("A batch ID is required")?;
            let state = batch::get(&paths.data_dir, id)?;
            ensure!(
                state.parent_session_id == session.id,
                "Resume the batch's originating session to manage it"
            );
            match action {
                "cancel" => {
                    ensure!(words.next().is_none(), "Usage: /batch cancel ID");
                    batch::cancel(&paths.data_dir, id)?;
                    eprintln!("Cancellation requested; inspect /batch show {id} for confirmed completion.");
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
                    let (output, error) = background_sessions::output(&paths.data_dir, job)?;
                    println!("{output}");
                    eprintln!("{error}");
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
            let state = batch::get(&paths.data_dir, id)?;
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
                    .create(&workspace, &session.model, &session.provider_id)?;
            crate::session_commands::copy_settings(&session.store.db, &session.id, &planner_id)?;
            if let Some(agent) =
                deepseek_code_core::services::agents::for_session(&session.store.db, &session.id)?
            {
                deepseek_code_core::services::agents::bind(&session.store.db, &planner_id, &agent)?;
            }
            let prompt=format!("Research this repository using read-only tools and plan the user's batch change: {instruction}\nReturn 5–30 roughly equal independent units, each mergeable without other units. Give each unit non-overlapping repository-relative file/directory paths (no traversal), a precise title and self-contained implementation instructions. Include observed code conventions in summary, and a concrete shared unit/e2e verification recipe. Do not fabricate repository findings. Do not modify files or run workers. Return only the requested structured plan.");
            let mut process = tokio::process::Command::new(&launch.executable);
            process
                .args(&launch.arguments)
                .args([
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
                    "--",
                    &prompt,
                ])
                .envs(environment)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            #[cfg(windows)]
            process.creation_flags(0x08000000);
            eprintln!("Researching a batch plan in a separate read-only session. No workers will start until /batch run ID.");
            let output = tokio::select! {result=tokio::time::timeout(Duration::from_secs(600),process.output())=>result.context("Batch planning timed out")??,_=tokio::signal::ctrl_c()=>anyhow::bail!("Batch planning cancelled")};
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
                &session.id,
                &workspace,
                instruction,
                proposal,
            )?);
        }
    }
    Ok(true)
}
