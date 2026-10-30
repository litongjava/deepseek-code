//! CLI-only recovery guidance; shared Desktop errors remain unchanged.
use deepseek_code_core::{db::DbManager, runtime::RuntimePaths};

pub fn repair_error(paths: &RuntimePaths, id: &str, error: anyhow::Error) -> anyhow::Error {
    if !error.to_string().contains("Cannot repair trust while") {
        return error;
    }
    error.context(format!("Stop the idle worker with:\n  dcl --data-dir {} sessions shutdown-worker {}\nIf tasks are still active, that command lists their IDs and explicit cancellation commands. No running work is stopped automatically.", quote(&paths.data_dir.to_string_lossy()), quote(id)))
}

fn shell_tasks(paths: &RuntimePaths, id: &str) -> anyhow::Result<Vec<serde_json::Value>> {
    let value = deepseek_code_core::tools::tasks::execute_for_session(
        "TaskList",
        serde_json::json!({}),
        &paths.data_dir,
        id,
    )?;
    Ok(value["tasks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|task| task["metadata"]["managed_shell"] == true)
        .cloned()
        .collect())
}

pub async fn shutdown_worker(paths: &RuntimePaths, id: &str) -> anyhow::Result<()> {
    uuid::Uuid::parse_str(id)?;
    let mut active = shell_tasks(paths, id)?
        .into_iter()
        .filter(|task| {
            !matches!(
                task["status"].as_str(),
                Some("completed" | "failed" | "cancelled")
            )
        })
        .filter_map(|task| task["id"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    active.extend(
        deepseek_code_core::services::background_sessions::list(&paths.data_dir, None)?
            .into_iter()
            .filter(|job| {
                job.session_id == id
                    && matches!(job.status.as_str(), "starting" | "running" | "unresponsive")
            })
            .map(|job| job.id),
    );
    anyhow::ensure!(active.is_empty(), "Session has active tasks: {}. Wait for completion or explicitly cancel each with: dcl --data-dir {} sessions stop-task {} TASK_ID", active.join(", "), quote(&paths.data_dir.to_string_lossy()), quote(id));
    deepseek_code_core::services::session_worker::shutdown(&paths.data_dir, id).await?;
    println!(
        "{}",
        serde_json::json!({"session_id":id,"worker":"stopped"})
    );
    Ok(())
}

pub async fn stop_task(paths: &RuntimePaths, id: &str, task_id: &str) -> anyhow::Result<()> {
    uuid::Uuid::parse_str(id)?;
    uuid::Uuid::parse_str(task_id)?;
    let value = if shell_tasks(paths, id)?
        .iter()
        .any(|task| task["id"] == task_id)
    {
        deepseek_code_core::tools::tasks::execute_for_session_async(
            "TaskStop",
            serde_json::json!({"task_id":task_id}),
            &paths.data_dir,
            id,
        )
        .await?
    } else {
        let job = deepseek_code_core::services::background_sessions::get(&paths.data_dir, task_id)?;
        anyhow::ensure!(
            job.session_id == id || job.parent_session_id == id,
            "Task belongs to another session"
        );
        serde_json::to_value(deepseek_code_core::services::background_sessions::cancel(
            &paths.data_dir,
            task_id,
        )?)?
    };
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

pub fn diagnose(
    db: &DbManager,
    paths: &RuntimePaths,
    id: &str,
    error: anyhow::Error,
) -> anyhow::Error {
    let detail = format!("{error:#}");
    if !detail.contains("Trust all project folders")
        && !detail.contains("Project folders have changed")
    {
        return error;
    }
    let Ok(project) = deepseek_code_core::services::session_trust::inspect(db, id) else {
        return error;
    };
    let mut message = format!("Session {id} needs explicit project-folder trust.\n");
    let mut command = format!(
        "dcl --data-dir {} sessions trust {}",
        quote(&paths.data_dir.to_string_lossy()),
        quote(id)
    );
    for folder in project.folders {
        let selected = deepseek_code_core::db::project_repo::normalize_folders(&[folder.path.clone()]);
        let (label, path) = match selected {
            Ok(current) if current[0] != folder.path => ("changed", current[0].clone()),
            Ok(_) => (
                if folder.trusted {
                    "trusted"
                } else {
                    "not trusted"
                },
                folder.path.clone(),
            ),
            Err(_) => (
                "unavailable; restore this directory first",
                folder.path.clone(),
            ),
        };
        message.push_str(&format!("  {} [{label}]\n", safe(&folder.path)));
        command.push_str(&format!(" --folder {}", quote(&path)));
    }
    message.push_str(&format!("Review every directory, then run:\n  {command}\nResume with:\n  dcl --data-dir {} --resume {}\nTrust repair affects only this session. A missing or changed source must be restored; it is not relocated by this command.", quote(&paths.data_dir.to_string_lossy()), quote(id)));
    error.context(message)
}

fn safe(value: &str) -> String {
    value
        .chars()
        .filter(|c| {
            !c.is_control() && !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .collect()
}
fn quote(value: &str) -> String {
    #[cfg(windows)]
    {
        format!("'{}'", safe(value).replace('\'', "''"))
    }
    #[cfg(not(windows))]
    {
        format!("'{}'", safe(value).replace('\'', "'\\''"))
    }
}
