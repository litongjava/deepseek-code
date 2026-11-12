//! Session-scoped application-data task records. No background worker completion is fabricated.
use anyhow::{bail, ensure, Context, Result};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::path::Path;

pub(super) fn definitions() -> Vec<super::ToolDefinition> {
    [
        ("TaskCreate", "Create a durable session task.", json!({"name":{"type":"string"},"priority":{"type":"string"},"subject":{"type":"string"},"description":{"type":"string"},"activeForm":{"type":"string"},"metadata":{"type":"object"}}), vec![]),
        ("TaskGet", "Read a durable session task by ID, including its available output and owner/dependency fields. For a managed background shell, subagent or workflow, set block:true to wait for it to finish; timeout is milliseconds (default 30000 when waiting, maximum 600000) and timeout:0 returns immediately. Without block:true the call returns the current snapshot immediately. retrieval_status reports whether this read completed (success/not_ready/timeout); it does not replace the task's own status, exit_code or error, and a timeout never stops the underlying process. Use TaskStop to cancel one.", json!({"id":{"type":"string","description":"Task ID."},"task_id":{"type":"string","description":"Alias for id."},"taskId":{"type":"string","description":"Alias for id."},"block":{"type":"boolean","default":false,"description":"Wait for a running managed background task to reach a terminal status."},"timeout":{"type":"integer","minimum":0,"maximum":600000,"description":"Wait limit in milliseconds when block is true; default 30000."}}),vec![]),
        ("TaskList", "List durable session tasks.", json!({}),vec![]),
        ("TaskUpdate", "Update a durable session task. Dependencies must exist and cannot form cycles. setBlockedBy atomically replaces the task's direct dependencies ([] clears them) and cannot be combined with addBlockedBy, addBlocks or deleting the task.", json!({"id":{"type":"string"},"taskId":{"type":"string"},"name":{"type":"string"},"priority":{"type":"string"},"subject":{"type":"string"},"description":{"type":"string"},"status":{"type":"string","enum":["pending","in_progress","running","completed","failed","cancelled","deleted"]},"output":{"type":"string"},"owner":{"type":"string"},"activeForm":{"type":"string"},"metadata":{"type":"object"},"addBlockedBy":{"type":"array","items":{"type":"string"}},"addBlocks":{"type":"array","items":{"type":"string"}},"setBlockedBy":{"type":"array","items":{"type":"string"}}}),vec![]),
        ("TodoWrite", "Replace the session's durable todo list.", json!({"todos":{"type":"array","items":{"type":"object","properties":{"content":{"type":"string"},"activeForm":{"type":"string"},"status":{"type":"string","enum":["pending","in_progress","completed"]}},"required":["content","status"]}}}),vec!["todos"]),
        ("TodoRead", "Read the session's durable todo list.",json!({}),vec![]),
        // `TaskOutput` is not advertised. Its dispatch alias remains below for
        // old transcripts and is not discoverable through ToolSearch.
        ("TaskStop", "\n- Stops a running background task by its ID\n- Takes a task_id parameter identifying the task to stop\n- Stops a background shell together with its descendant processes; for other task records it marks the record cancelled\n- Returns a success or failure status\n- Use this tool when you need to terminate a long-running task\n",json!({"task_id":{"description":"The ID of the background task to stop.","type":"string"},"shell_id":{"description":"Deprecated: use task_id instead","type":"string"}}),vec![]),
    ].into_iter().map(|(name,description,properties,required)| super::ToolDefinition {name:name.into(),description:description.into(),input_schema:json!({"type":"object","properties":properties,"required":required})}).collect()
}

pub fn is_task(name: &str) -> bool {
    matches!(
        name,
        "TaskCreate"
            | "TaskGet"
            | "TaskList"
            | "TaskUpdate"
            | "TaskOutput"
            | "TaskStop"
            | "TodoWrite"
            | "TodoRead"
    )
}

fn readonly(name: &str) -> bool {
    matches!(name, "TaskGet" | "TaskList" | "TodoRead" | "TaskOutput")
}

/// Read only the owning session's title; never open logs or create task storage.
pub fn display_title(root: &Path, session: &str, input: &Value) -> Option<String> {
    let session = uuid::Uuid::parse_str(session).ok()?.to_string();
    let id = input["task_id"]
        .as_str()
        .or_else(|| input["taskId"].as_str())
        .or_else(|| input["id"].as_str())
        .or_else(|| input["shell_id"].as_str())?;
    let title = if uuid::Uuid::parse_str(id).is_ok()
        && root
            .join("background-tasks")
            .join(id)
            .join("state.json")
            .is_file()
    {
        let job = crate::services::background_sessions::get(root, id).ok()?;
        if job.parent_session_id != session {
            return None;
        }
        job.title
    } else {
        let db = rusqlite::Connection::open_with_flags(
            root.join("tasks").join(format!("{session}.sqlite3")),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .ok()?;
        db.busy_timeout(std::time::Duration::from_millis(25)).ok()?;
        let body: String = db
            .query_row("SELECT body FROM tasks WHERE id=?1", [id], |row| row.get(0))
            .ok()?;
        let task: Value = serde_json::from_str(&body).ok()?;
        task["subject"].as_str()?.to_owned()
    };
    let title: String = title
        .chars()
        .filter(|ch| !ch.is_control())
        .take(160)
        .collect();
    (!title.trim().is_empty()).then(|| title.trim().into())
}

pub fn execute_for_session(name: &str, input: Value, root: &Path, session: &str) -> Result<Value> {
    let session = uuid::Uuid::parse_str(session).context("Invalid task session ID")?;
    if let Some(output) =
        super::background_shell::control(name, &input, root, &session.to_string())?
    {
        return Ok(output);
    }
    execute_at(
        name,
        input,
        &root.join("tasks").join(format!("{session}.sqlite3")),
    )
}

pub async fn execute_for_session_async(
    name: &str,
    input: Value,
    root: &Path,
    session: &str,
) -> Result<Value> {
    uuid::Uuid::parse_str(session).context("Invalid task session ID")?;
    // The legacy TaskOutput name kept its old default: wait for completion.
    let mut input = input;
    if name == "TaskOutput" && input.get("block").is_none() {
        input["block"] = json!(true);
    }
    if matches!(name, "TaskGet" | "TaskOutput" | "TaskStop") {
        if let Some(id) = input["task_id"]
            .as_str()
            .or_else(|| input["taskId"].as_str())
            .or_else(|| input["id"].as_str())
        {
            if uuid::Uuid::parse_str(id).is_ok()
                && root
                    .join("background-tasks")
                    .join(id)
                    .join("state.json")
                    .exists()
            {
                use crate::services::background_sessions;
                let (read_root, read_id) = (root.to_owned(), id.to_owned());
                let mut job = tokio::task::spawn_blocking(move || {
                    background_sessions::get(&read_root, &read_id)
                })
                .await??;
                ensure!(
                    job.parent_session_id == session,
                    "Task belongs to another session"
                );
                if name == "TaskStop" {
                    let (root, id) = (root.to_owned(), id.to_owned());
                    let mut value = tokio::task::spawn_blocking(move || {
                        background_sessions::cancel(&root, &id)?;
                        job_snapshot(&root, &id)
                    })
                    .await??;
                    let status = value["status"].as_str().unwrap_or_default();
                    value["retrieval_status"] = json!(if terminal_status(status) {
                        "success"
                    } else {
                        "not_ready"
                    });
                    return Ok(value);
                }
                let (block, timeout) = super::background_shell::wait_policy(&input)?;
                let deadline = tokio::time::Instant::now() + timeout;
                let mut timed_out = false;
                if block {
                    loop {
                        if !matches!(job.status.as_str(), "starting" | "running") {
                            break;
                        }
                        let now = tokio::time::Instant::now();
                        if now >= deadline {
                            timed_out = true;
                            break;
                        }
                        tokio::time::sleep_until(
                            (now + std::time::Duration::from_millis(100)).min(deadline),
                        )
                        .await;
                        let (root, id) = (root.to_owned(), id.to_owned());
                        job = tokio::task::spawn_blocking(move || {
                            background_sessions::get(&root, &id)
                        })
                        .await??;
                    }
                }
                // The output files and the optional SQLite report are read on
                // the blocking pool. An explicit wait deadline bounds the whole
                // retrieval, so a slow disk or busy database cannot outlive it.
                // A wait that used its whole deadline still gets a short bounded
                // read, so the caller sees output already written; a busy disk or
                // database still cannot hold the call open for long.
                let (read_root, read_id) = (root.to_owned(), id.to_owned());
                let snapshot = tokio::task::spawn_blocking(move || {
                    job_snapshot(&read_root, &read_id)
                });
                let snapshot = if block {
                    let read_deadline = deadline
                        .max(tokio::time::Instant::now() + std::time::Duration::from_secs(2));
                    match tokio::time::timeout_at(read_deadline, snapshot).await {
                        Ok(result) => Some(result??),
                        Err(_) => {
                            timed_out = true;
                            None
                        }
                    }
                } else {
                    Some(snapshot.await??)
                };
                let (status, mut value) = match snapshot {
                    Some(value) => {
                        let status = value["status"].as_str().unwrap_or_default().to_owned();
                        (status, value)
                    }
                    None => (
                        job.status.clone(),
                        json!({"task_id":job.id,"subject":job.title,"status":job.status,"stdout":"","stderr":"","exit_code":job.exit_code,"error":job.error}),
                    ),
                };
                // Retrieval success only says the read completed. The job's
                // own status/exit_code/error stay separate and unchanged.
                let retrieval_status = if timed_out {
                    "timeout"
                } else if terminal_status(&status) {
                    "success"
                } else {
                    "not_ready"
                };
                value["retrieval_status"] = json!(retrieval_status);
                return Ok(value);
            }
        }
    }
    if let Some(output) =
        super::background_shell::control_async(name, &input, root, session).await?
    {
        return Ok(output);
    }
    let (name, root, session) = (name.to_owned(), root.to_owned(), session.to_owned());
    tokio::task::spawn_blocking(move || execute_for_session(&name, input, &root, &session)).await?
}

/// Read a detached worker job's current output. The assistant report is
/// supplementary: a locked or damaged database must not fail the retrieval.
fn job_snapshot(root: &Path, id: &str) -> Result<Value> {
    use crate::services::background_sessions;
    let job = background_sessions::get(root, id)?;
    let (stdout, stderr) = background_sessions::output(root, id)?;
    let mut stdout = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|event| {
            event["result"]
                .as_str()
                .or_else(|| event["delta"].as_str())
                .map(str::to_owned)
        })
        .collect::<Vec<_>>()
        .join("\n");
    if root.join(crate::runtime::DATABASE_FILE).exists() {
        if let Ok(execution) = crate::services::session_worker::job_execution_session(root, &job) {
            if let Ok(conn) = rusqlite::Connection::open_with_flags(
                root.join(crate::runtime::DATABASE_FILE),
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            ) {
                let report: Option<String> = conn
                    .query_row("SELECT content FROM messages WHERE conversation_id=?1 AND role='assistant' AND content<>'' ORDER BY sort_order DESC LIMIT 1", [&execution], |row| row.get(0))
                    .optional()
                    .unwrap_or(None);
                if let Some(report) = report {
                    stdout = report;
                }
            }
        }
    }
    let id = job.id.clone();
    Ok(
        json!({"id":id,"task_id":job.id,"subject":job.title,"status":job.status,"stdout":stdout,"stderr":stderr,"exit_code":job.exit_code,"error":job.error}),
    )
}

fn terminal_status(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "cancelled")
}

/// Read one stored session task without dispatching background controls.
/// Callers that own a managed shell use this to avoid re-entering `control`.
pub(crate) fn read_for_session(id: &str, root: &Path, session: &str) -> Result<Value> {
    let session = uuid::Uuid::parse_str(session).context("Invalid task session ID")?;
    execute_at(
        "TaskGet",
        json!({"id":id}),
        &root.join("tasks").join(format!("{session}.sqlite3")),
    )
}

pub fn read_legacy(name: &str, input: Value, cwd: &str) -> Result<Value> {
    ensure!(readonly(name), "Legacy project tasks are read-only");
    let path = Path::new(cwd).canonicalize()?.join(".claude/tasks.sqlite3");
    super::resolve_path(&path.to_string_lossy(), cwd)?;
    execute_at(name, input, &path)
}

pub(super) fn execute(name: &str, input: Value, cwd: &str) -> Result<Value> {
    let workspace = Path::new(cwd).canonicalize()?;
    let hash = ring::digest::digest(
        &ring::digest::SHA256,
        workspace.to_string_lossy().as_bytes(),
    );
    let key = hash
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let root = crate::runtime::RuntimePaths::default_data_dir()?;
    execute_at(
        name,
        input,
        &root.join("tasks/projects").join(format!("{key}.sqlite3")),
    )
}

/// Internal process owner API. This authority is never read from model input.
pub(crate) fn execute_for_shell_owner(
    name: &str,
    input: Value,
    root: &Path,
    session: &str,
) -> Result<Value> {
    ensure!(
        matches!(name, "TaskCreate" | "TaskUpdate"),
        "Invalid shell owner operation"
    );
    let session = uuid::Uuid::parse_str(session).context("Invalid task session ID")?;
    execute_at_owned(
        name,
        input,
        &root.join("tasks").join(format!("{session}.sqlite3")),
        true,
    )
}

fn execute_at(name: &str, input: Value, path: &Path) -> Result<Value> {
    execute_at_owned(name, input, path, false)
}
fn execute_at_owned(name: &str, mut input: Value, path: &Path, shell_owner: bool) -> Result<Value> {
    // `TaskOutput` was folded into `TaskGet`. Old transcripts may still
    // dispatch it; it goes through the same validation, storage and session
    // path and is not advertised or loadable through ToolSearch.
    let name = if name == "TaskOutput" {
        "TaskGet"
    } else {
        name
    };
    if name == "TaskGet" {
        // Validate block/timeout even for plain records so a bad wait request
        // is reported instead of silently ignored.
        super::background_shell::wait_policy(&input)?;
    }
    if !shell_owner {
        ensure!(
            input
                .get("metadata")
                .and_then(|m| m.get("managed_shell"))
                .is_none(),
            "managed_shell provenance is reserved for the shell process owner"
        );
    }
    // shell_id is the reference CLI's deprecated TaskStop alias.
    if input.get("id").is_none() {
        if let Some(id) = input
            .get("task_id")
            .or_else(|| input.get("shell_id"))
            .cloned()
        {
            input["id"] = id;
        }
    }
    if input.get("taskId").is_none() {
        if let Some(id) = input.get("id").cloned() {
            input["taskId"] = id;
        }
    }
    if input.get("subject").is_none() {
        if let Some(subject) = input.get("name").cloned() {
            input["subject"] = subject;
        }
    }
    if name == "TaskStop" {
        input["status"] = json!("cancelled");
        let task = execute_at("TaskUpdate", input, path)?;
        return Ok(json!({"task":task,"record_cancelled":true,"worker_process_stopped":false}));
    }
    let directory = path.parent().context("Task storage parent missing")?;
    let readonly = readonly(name);
    if readonly && !path.exists() {
        return match name {
            "TaskList" => Ok(json!({"tasks":[]})),
            "TodoRead" => Ok(json!({"todos":[]})),
            _ => bail!("Task not found"),
        };
    }
    if !readonly {
        std::fs::create_dir_all(&directory)?;
    }
    let mut db = if readonly {
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?
    } else {
        rusqlite::Connection::open(path)?
    };
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    if !readonly {
        db.execute_batch("CREATE TABLE IF NOT EXISTS tasks(id TEXT PRIMARY KEY, body TEXT NOT NULL); CREATE TABLE IF NOT EXISTS todo(singleton INTEGER PRIMARY KEY CHECK(singleton=1), body TEXT NOT NULL);")?;
    }
    let tx = db.transaction_with_behavior(if readonly {
        rusqlite::TransactionBehavior::Deferred
    } else {
        rusqlite::TransactionBehavior::Immediate
    })?;
    let get = |id: &str| -> Result<Value> {
        let body: Option<String> = tx
            .query_row("SELECT body FROM tasks WHERE id=?1", [id], |row| row.get(0))
            .optional()?;
        Ok(serde_json::from_str(
            &body.with_context(|| format!("Task not found: {id}"))?,
        )?)
    };
    let result = match name {
        "TaskCreate" => {
            let subject = input["subject"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .context("subject is required")?;
            let description = input["description"].as_str().unwrap_or("");
            let priority = input["priority"].as_str().unwrap_or("medium");
            if !matches!(priority, "low" | "medium" | "high" | "critical") {
                bail!("Invalid task priority");
            }
            let id = uuid::Uuid::new_v4().to_string();
            let mut task = json!({"id":id,"subject":subject,"description":description,"status":"pending","blockedBy":[],"blocks":[],"createdAt":chrono::Utc::now().to_rfc3339()});
            task["name"] = json!(subject);
            task["priority"] = json!(priority);
            task["output"] = Value::Null;
            for key in ["activeForm", "metadata"] {
                if let Some(value) = input.get(key) {
                    task[key] = value.clone();
                }
            }
            tx.execute(
                "INSERT INTO tasks(id,body) VALUES(?1,?2)",
                params![id, task.to_string()],
            )?;
            task
        }
        // An ordinary record has no live process. Return it exactly as stored
        // so existing task readers keep their field set; managed jobs add
        // retrieval_status on their own snapshot path.
        "TaskGet" => get(input["taskId"].as_str().context("taskId is required")?)?,
        "TaskList" => {
            let mut statement = tx.prepare("SELECT body FROM tasks ORDER BY rowid")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            let mut tasks = vec![];
            for row in rows {
                tasks.push(serde_json::from_str::<Value>(&row?)?);
            }
            json!({"tasks":tasks})
        }
        "TaskUpdate" => {
            let id = input["taskId"].as_str().context("taskId is required")?;
            let mut task = get(id)?;
            // Replacing dependencies and appending or deleting are mutually
            // exclusive: combining them would make the result depend on the
            // order they happened to be applied in.
            if input.get("setBlockedBy").is_some() {
                ensure!(
                    input.get("addBlockedBy").is_none() && input.get("addBlocks").is_none(),
                    "setBlockedBy cannot be combined with addBlockedBy or addBlocks"
                );
                ensure!(
                    input["status"] != "deleted",
                    "setBlockedBy cannot be combined with deleting the task"
                );
            }
            if !shell_owner && task["metadata"]["managed_shell"] == true {
                ensure!(
                    input.get("status").is_none() && input.get("output").is_none(),
                    "Managed shell status/output/deletion are owned by the running process; use TaskStop to cancel"
                );
            }
            if input["status"] == "deleted" {
                let mut statement = tx.prepare("SELECT id,body FROM tasks WHERE id!=?1")?;
                let rows = statement
                    .query_map([id], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                drop(statement);
                for (other_id, body) in rows {
                    let mut other: Value = serde_json::from_str(&body)?;
                    for field in ["blockedBy", "blocks"] {
                        other[field]
                            .as_array_mut()
                            .context("Invalid stored dependencies")?
                            .retain(|value| value != id);
                    }
                    tx.execute(
                        "UPDATE tasks SET body=?2 WHERE id=?1",
                        params![other_id, other.to_string()],
                    )?;
                }
                tx.execute("DELETE FROM tasks WHERE id=?1", [id])?;
                json!({"id":id,"deleted":true})
            } else {
                for key in [
                    "subject",
                    "description",
                    "status",
                    "owner",
                    "activeForm",
                    "output",
                ] {
                    if let Some(value) = input.get(key) {
                        if !value.is_string() {
                            bail!("{key} must be a string");
                        }
                        task[key] = value.clone();
                    }
                }
                if !matches!(
                    task["status"].as_str(),
                    Some(
                        "pending"
                            | "in_progress"
                            | "running"
                            | "completed"
                            | "failed"
                            | "cancelled"
                    )
                ) {
                    bail!("Invalid task status");
                }
                if let Some(metadata) = input.get("metadata") {
                    let updates = metadata.as_object().context("metadata must be an object")?;
                    if !task["metadata"].is_object() {
                        task["metadata"] = json!({});
                    }
                    for (key, value) in updates {
                        if value.is_null() {
                            task["metadata"].as_object_mut().unwrap().remove(key);
                        } else {
                            task["metadata"][key] = value.clone();
                        }
                    }
                }
                if let Some(values) = input.get("setBlockedBy") {
                    let values = values.as_array().context("setBlockedBy must be an array")?;
                    let mut desired: Vec<String> = Vec::new();
                    for value in values {
                        let other_id = value.as_str().context("Dependency must be a task ID")?;
                        if other_id == id {
                            bail!("Task cannot depend on itself");
                        }
                        // Repeated IDs are one edge; the reciprocal list must
                        // not accumulate duplicates either.
                        if !desired.iter().any(|existing| existing.as_str() == other_id) {
                            desired.push(other_id.to_owned());
                        }
                    }
                    // Validate the complete replacement before touching any row.
                    // Following blockedBy from each new dependency finds this
                    // task exactly when the final graph would contain a cycle;
                    // removed edges cannot create one.
                    for other_id in &desired {
                        get(other_id)?;
                        let mut pending = vec![other_id.clone()];
                        let mut visited = std::collections::HashSet::new();
                        while let Some(next) = pending.pop() {
                            if next == id {
                                bail!("Task dependency cycle");
                            }
                            if visited.insert(next.clone()) {
                                let node = get(&next)?;
                                for edge in node["blockedBy"]
                                    .as_array()
                                    .context("Invalid stored dependencies")?
                                {
                                    pending.push(
                                        edge.as_str()
                                            .context("Invalid stored dependency")?
                                            .to_string(),
                                    );
                                }
                            }
                        }
                    }
                    let mut current: Vec<String> = Vec::new();
                    for value in task["blockedBy"]
                        .as_array()
                        .context("Invalid stored dependencies")?
                    {
                        let dependency = value.as_str().context("Invalid stored dependency")?;
                        if !current
                            .iter()
                            .any(|existing| existing.as_str() == dependency)
                        {
                            current.push(dependency.to_owned());
                        }
                    }
                    // Direct dependencies of this task change; every other
                    // stored list stays as written. Only edges that really
                    // changed are updated, so an identical replacement writes
                    // no other row.
                    for removed in current
                        .iter()
                        .filter(|dependency| !desired.iter().any(|edge| edge == *dependency))
                    {
                        let mut other = get(removed)?;
                        let edges = other["blocks"]
                            .as_array_mut()
                            .context("Invalid stored dependencies")?;
                        if edges.iter().any(|edge| edge.as_str() == Some(id)) {
                            edges.retain(|edge| edge.as_str() != Some(id));
                            tx.execute(
                                "UPDATE tasks SET body=?2 WHERE id=?1",
                                params![removed, other.to_string()],
                            )?;
                        }
                    }
                    for added in desired
                        .iter()
                        .filter(|dependency| !current.iter().any(|edge| edge == *dependency))
                    {
                        let mut other = get(added)?;
                        let edges = other["blocks"]
                            .as_array_mut()
                            .context("Invalid stored dependencies")?;
                        if !edges.iter().any(|edge| edge.as_str() == Some(id)) {
                            edges.push(json!(id));
                            tx.execute(
                                "UPDATE tasks SET body=?2 WHERE id=?1",
                                params![added, other.to_string()],
                            )?;
                        }
                    }
                    task["blockedBy"] = json!(desired);
                }
                for (parameter, field, inverse) in [
                    ("addBlockedBy", "blockedBy", "blocks"),
                    ("addBlocks", "blocks", "blockedBy"),
                ] {
                    if let Some(values) = input.get(parameter) {
                        for value in values.as_array().context("Dependencies must be arrays")? {
                            let other_id =
                                value.as_str().context("Dependency must be a task ID")?;
                            if other_id == id {
                                bail!("Task cannot depend on itself");
                            }
                            let mut other = get(other_id)?;
                            // Traversing the opposite edge detects cycles before adding this edge.
                            let mut pending = vec![other_id.to_string()];
                            let mut visited = std::collections::HashSet::new();
                            while let Some(next) = pending.pop() {
                                if next == id {
                                    bail!("Task dependency cycle");
                                }
                                if visited.insert(next.clone()) {
                                    let node = get(&next)?;
                                    for edge in node[field]
                                        .as_array()
                                        .context("Invalid stored dependencies")?
                                    {
                                        pending.push(
                                            edge.as_str()
                                                .context("Invalid stored dependency")?
                                                .to_string(),
                                        );
                                    }
                                }
                            }
                            let edges = task[field]
                                .as_array_mut()
                                .context("Invalid stored dependencies")?;
                            if !edges.contains(value) {
                                edges.push(value.clone());
                            }
                            let edges = other[inverse]
                                .as_array_mut()
                                .context("Invalid stored dependencies")?;
                            if !edges.contains(&json!(id)) {
                                edges.push(json!(id));
                            }
                            tx.execute(
                                "UPDATE tasks SET body=?2 WHERE id=?1",
                                params![other_id, other.to_string()],
                            )?;
                        }
                    }
                }
                task["updatedAt"] = json!(chrono::Utc::now().to_rfc3339());
                tx.execute(
                    "UPDATE tasks SET body=?2 WHERE id=?1",
                    params![id, task.to_string()],
                )?;
                task
            }
        }
        "TodoWrite" => {
            let todos = input["todos"]
                .as_array()
                .context("todos must be an array")?;
            for todo in todos {
                if todo["content"].as_str().is_none_or(|s| s.trim().is_empty())
                    || !matches!(
                        todo["status"].as_str(),
                        Some("pending" | "in_progress" | "completed")
                    )
                {
                    bail!("Invalid todo content or status");
                }
            }
            tx.execute("INSERT INTO todo(singleton,body) VALUES(1,?1) ON CONFLICT(singleton) DO UPDATE SET body=excluded.body",[json!(todos).to_string()])?;
            json!({"todos":todos})
        }
        "TodoRead" => {
            let body: Option<String> = tx
                .query_row("SELECT body FROM todo WHERE singleton=1", [], |row| {
                    row.get(0)
                })
                .optional()?;
            json!({"todos":serde_json::from_str::<Value>(body.as_deref().unwrap_or("[]"))?})
        }
        _ => bail!("Unknown task tool: {name}"),
    };
    tx.commit()?;
    Ok(result)
}

fn validate_todo(todo: &Value) -> Result<()> {
    if todo["content"].as_str().is_none_or(|s| s.trim().is_empty())
        || !matches!(
            todo["status"].as_str(),
            Some("pending" | "in_progress" | "completed" | "cancelled")
        )
        || !matches!(
            todo["priority"].as_str(),
            Some("low" | "medium" | "high" | "urgent")
        )
        || todo["tags"]
            .as_array()
            .is_none_or(|tags| tags.iter().any(|tag| !tag.is_string()))
    {
        bail!("Invalid todo fields");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn detached_agent_lookup_reads_report_and_rejects_other_sessions() {
        let root = tempfile::tempdir().unwrap();
        let parent = uuid::Uuid::new_v4().to_string();
        let id = uuid::Uuid::new_v4().to_string();
        let dir = root.path().join("background-tasks").join(&id);
        std::fs::create_dir_all(&dir).unwrap();
        let job = json!({"id":id,"parent_session_id":parent,"session_id":uuid::Uuid::new_v4().to_string(),"title":"Review changes","status":"completed","supervisor_pid":null,"child_pid":null,"started_ms":0,"heartbeat_ms":0,"finished_ms":1,"exit_code":0,"error":null});
        std::fs::write(dir.join("state.json"), job.to_string()).unwrap();
        assert_eq!(
            display_title(root.path(), &parent, &json!({"task_id":id})).as_deref(),
            Some("Review changes")
        );
        assert!(display_title(
            root.path(),
            &uuid::Uuid::new_v4().to_string(),
            &json!({"task_id":id})
        )
        .is_none());
        std::fs::write(
            dir.join("output.jsonl"),
            "{\"type\":\"result\",\"result\":\"Review report\"}\n",
        )
        .unwrap();
        for name in ["TaskGet", "TaskOutput"] {
            let output = execute_for_session_async(
                name,
                json!({"task_id":id,"block":false}),
                root.path(),
                &parent,
            )
            .await
            .unwrap();
            assert_eq!(output["subject"], "Review changes");
            assert_eq!(output["stdout"], "Review report");
            assert!(execute_for_session_async(
                name,
                json!({"task_id":id}),
                root.path(),
                &uuid::Uuid::new_v4().to_string()
            )
            .await
            .is_err());
        }
    }
    #[test]
    fn task_output_is_not_advertised_and_task_get_carries_wait_parameters() {
        let definitions = definitions();
        for name in ["TaskCreate", "TaskGet", "TaskUpdate", "TaskList"] {
            assert_eq!(
                definitions
                    .iter()
                    .filter(|definition| definition.name == name)
                    .count(),
                1,
                "{name} must stay a registered built-in"
            );
        }
        assert!(
            definitions
                .iter()
                .all(|definition| definition.name != "TaskOutput"),
            "TaskOutput must not be model-visible"
        );
        let get = definitions
            .iter()
            .find(|definition| definition.name == "TaskGet")
            .unwrap();
        for key in ["id", "task_id", "taskId", "block", "timeout"] {
            assert!(
                get.input_schema["properties"].get(key).is_some(),
                "TaskGet is missing {key}"
            );
        }
        assert_eq!(get.input_schema["properties"]["block"]["type"], "boolean");
        assert_eq!(get.input_schema["properties"]["block"]["default"], false);
        assert_eq!(get.input_schema["properties"]["timeout"]["type"], "integer");
        assert_eq!(get.input_schema["properties"]["timeout"]["minimum"], 0);
        assert_eq!(
            get.input_schema["properties"]["timeout"]["maximum"],
            600_000
        );
        let update = definitions
            .iter()
            .find(|definition| definition.name == "TaskUpdate")
            .unwrap();
        assert_eq!(
            update.input_schema["properties"]["setBlockedBy"]["type"],
            "array"
        );
        assert_eq!(
            update.input_schema["properties"]["setBlockedBy"]["items"]["type"],
            "string"
        );
        assert!(update.description.contains("setBlockedBy"));
    }

    #[test]
    fn set_blocked_by_replaces_dependencies_and_keeps_inverse_edges() {
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let create = |subject: &str| {
            execute_for_session(
                "TaskCreate",
                json!({"subject":subject}),
                root.path(),
                &session,
            )
            .unwrap()
        };
        let a = create("a");
        let b = create("b");
        let c = create("c");
        let id = a["id"].as_str().unwrap();
        let read = |task: &Value| {
            execute_for_session("TaskGet", json!({"id":task["id"]}), root.path(), &session).unwrap()
        };
        execute_for_session(
            "TaskUpdate",
            json!({"id":id,"addBlockedBy":[b["id"]]}),
            root.path(),
            &session,
        )
        .unwrap();
        assert_eq!(read(&b)["blocks"], json!([id]));

        // Replacing the direct dependency removes the old inverse edge and
        // adds the new one.
        execute_for_session(
            "TaskUpdate",
            json!({"id":id,"setBlockedBy":[c["id"]]}),
            root.path(),
            &session,
        )
        .unwrap();
        assert_eq!(read(&a)["blockedBy"], json!([c["id"]]));
        assert_eq!(read(&b)["blocks"], json!([]));
        assert_eq!(read(&c)["blocks"], json!([id]));

        // Duplicate IDs collapse to one edge, and an identical replacement is
        // idempotent.
        for _ in 0..2 {
            execute_for_session(
                "TaskUpdate",
                json!({"id":id,"setBlockedBy":[c["id"],c["id"]]}),
                root.path(),
                &session,
            )
            .unwrap();
        }
        assert_eq!(read(&a)["blockedBy"], json!([c["id"]]));
        assert_eq!(read(&c)["blocks"], json!([id]));

        // A freshly reopened connection reads the same persisted graph.
        let reopened = uuid::Uuid::parse_str(&session).unwrap().to_string();
        assert_eq!(
            execute_for_session("TaskGet", json!({"id":id}), root.path(), &reopened).unwrap()
                ["blockedBy"],
            json!([c["id"]])
        );
        assert_eq!(
            execute_for_session("TaskGet", json!({"id":c["id"]}), root.path(), &reopened).unwrap()
                ["blocks"],
            json!([id])
        );

        // The empty array clears the dependency and its inverse edge.
        execute_for_session(
            "TaskUpdate",
            json!({"id":id,"setBlockedBy":[]}),
            root.path(),
            &session,
        )
        .unwrap();
        assert_eq!(read(&a)["blockedBy"], json!([]));
        assert_eq!(read(&c)["blocks"], json!([]));
    }

    #[test]
    fn set_blocked_by_failures_leave_every_row_unchanged() {
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let create = |subject: &str| {
            execute_for_session(
                "TaskCreate",
                json!({"subject":subject}),
                root.path(),
                &session,
            )
            .unwrap()
        };
        let a = create("a");
        let b = create("b");
        let c = create("c");
        let id = a["id"].as_str().unwrap();
        execute_for_session(
            "TaskUpdate",
            json!({"id":id,"addBlockedBy":[b["id"]]}),
            root.path(),
            &session,
        )
        .unwrap();
        let snapshot = execute_for_session("TaskList", json!({}), root.path(), &session).unwrap();
        let missing = uuid::Uuid::new_v4().to_string();
        let cases = [
            // A dependency that does not exist in this session.
            json!({"id":id,"setBlockedBy":[missing]}),
            // Self reference.
            json!({"id":id,"setBlockedBy":[id]}),
            // a depends on b, so b depending on a would cycle.
            json!({"id":b["id"],"setBlockedBy":[id]}),
            // A valid replacement plus a missing dependency and a metadata
            // edit must not persist any part of the call.
            json!({"id":id,"description":"changed","setBlockedBy":[c["id"],missing]}),
            // Ambiguous dependency mutations are refused before writing.
            json!({"id":id,"setBlockedBy":[c["id"]],"status":"deleted"}),
            json!({"id":id,"setBlockedBy":[c["id"]],"addBlockedBy":[c["id"]]}),
            json!({"id":id,"setBlockedBy":[c["id"]],"addBlocks":[c["id"]]}),
            // Wrong types are rejected, not coerced.
            json!({"id":id,"setBlockedBy":"not-an-array"}),
            json!({"id":id,"setBlockedBy":[1]}),
        ];
        for case in cases {
            assert!(
                execute_for_session("TaskUpdate", case.clone(), root.path(), &session).is_err(),
                "invalid dependency replacement was accepted: {case}"
            );
            assert_eq!(
                execute_for_session("TaskList", json!({}), root.path(), &session).unwrap(),
                snapshot,
                "a failed update changed rows: {case}"
            );
        }

        // Another session's task store cannot reference this session's task.
        let foreign = uuid::Uuid::new_v4().to_string();
        let foreign_task = execute_for_session(
            "TaskCreate",
            json!({"subject":"foreign"}),
            root.path(),
            &foreign,
        )
        .unwrap();
        let error = execute_for_session(
            "TaskUpdate",
            json!({"id":foreign_task["id"],"setBlockedBy":[id]}),
            root.path(),
            &foreign,
        )
        .unwrap_err();
        assert!(error.to_string().contains("Task not found"), "{error}");
        assert_eq!(
            execute_for_session("TaskList", json!({}), root.path(), &session).unwrap(),
            snapshot
        );
        assert_eq!(
            execute_for_session(
                "TaskGet",
                json!({"id":foreign_task["id"]}),
                root.path(),
                &foreign
            )
            .unwrap()["blockedBy"],
            json!([])
        );
    }

    #[test]
    fn ordinary_task_get_keeps_fields_and_validates_wait_parameters() {
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let dependency = execute_for_session(
            "TaskCreate",
            json!({"subject":"dependency"}),
            root.path(),
            &session,
        )
        .unwrap();
        let task = execute_for_session(
            "TaskCreate",
            json!({"subject":"ordinary task","description":"details","priority":"high","metadata":{"note":"kept"}}),
            root.path(),
            &session,
        )
        .unwrap();
        let id = task["id"].as_str().unwrap();
        execute_for_session(
            "TaskUpdate",
            json!({"id":id,"owner":"alice","status":"running","output":"saved output","addBlockedBy":[dependency["id"]]}),
            root.path(),
            &session,
        )
        .unwrap();
        for key in ["id", "task_id", "taskId"] {
            let value = execute_for_session(
                "TaskGet",
                json!({key:id,"block":false}),
                root.path(),
                &session,
            )
            .unwrap();
            assert_eq!(value["id"], id);
            assert_eq!(value["subject"], "ordinary task");
            assert_eq!(value["description"], "details");
            assert_eq!(value["priority"], "high");
            assert_eq!(value["owner"], "alice");
            assert_eq!(value["output"], "saved output");
            assert_eq!(value["metadata"]["note"], "kept");
            assert_eq!(value["blockedBy"], json!([dependency["id"]]));
            assert!(value.get("retrieval_status").is_none());
        }
        // An ordinary record has no live process to wait for: block must not
        // hold the caller for the requested deadline.
        let started = std::time::Instant::now();
        let value = execute_for_session(
            "TaskGet",
            json!({"task_id":id,"block":true,"timeout":600_000}),
            root.path(),
            &session,
        )
        .unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert_eq!(value["status"], "running");
        assert!(value.get("retrieval_status").is_none());
        for bad in [
            json!({"timeout":-1}),
            json!({"timeout":1.5}),
            json!({"timeout":"100"}),
            json!({"timeout":600_001}),
            json!({"block":"true"}),
            json!({"block":1}),
        ] {
            let mut input = bad;
            input["task_id"] = json!(id);
            assert!(
                execute_for_session("TaskGet", input, root.path(), &session).is_err(),
                "invalid wait parameter was accepted"
            );
        }
    }

    #[tokio::test]
    async fn background_job_task_get_waits_and_preserves_failure_and_cancel() {
        use std::time::{SystemTime, UNIX_EPOCH};
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let id = uuid::Uuid::new_v4().to_string();
        let dir = root.path().join("background-tasks").join(&id);
        std::fs::create_dir_all(&dir).unwrap();
        let now = || {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64
        };
        let write = |status: &str, exit_code: Option<i32>, error: Option<&str>| {
            let job = json!({"id":id,"foreground":false,"parent_session_id":session,"session_id":uuid::Uuid::new_v4().to_string(),"title":"Review changes","status":status,"supervisor_pid":null,"child_pid":null,"started_ms":now(),"heartbeat_ms":now(),"finished_ms":null,"exit_code":exit_code,"error":error});
            std::fs::write(dir.join("state.json"), job.to_string()).unwrap();
        };
        write("running", None, None);
        std::fs::write(
            dir.join("output.jsonl"),
            "{\"type\":\"delta\",\"delta\":\"working\"}\n",
        )
        .unwrap();
        let snapshot = execute_for_session_async(
            "TaskGet",
            json!({"task_id":id,"block":false}),
            root.path(),
            &session,
        )
        .await
        .unwrap();
        assert_eq!(snapshot["status"], "running");
        assert_eq!(snapshot["retrieval_status"], "not_ready");
        assert_eq!(snapshot["stdout"], "working");
        // The wait ends at the deadline and leaves the job alone.
        let started = tokio::time::Instant::now();
        let timed = execute_for_session_async(
            "TaskGet",
            json!({"task_id":id,"block":true,"timeout":60}),
            root.path(),
            &session,
        )
        .await
        .unwrap();
        assert!(started.elapsed() >= std::time::Duration::from_millis(60));
        assert_eq!(timed["status"], "running");
        assert_eq!(timed["retrieval_status"], "timeout");
        assert!(!dir.join("cancel").exists());
        write("failed", Some(7), Some("worker failed"));
        std::fs::write(
            dir.join("output.jsonl"),
            "{\"type\":\"result\",\"result\":\"failure report\"}\n",
        )
        .unwrap();
        let failed = execute_for_session_async(
            "TaskGet",
            json!({"task_id":id,"block":true,"timeout":5_000}),
            root.path(),
            &session,
        )
        .await
        .unwrap();
        assert_eq!(failed["retrieval_status"], "success");
        assert_eq!(failed["status"], "failed");
        assert_eq!(failed["exit_code"], 7);
        assert_eq!(failed["error"], "worker failed");
        assert_eq!(failed["stdout"], "failure report");
        write("cancelled", None, None);
        let cancelled = execute_for_session_async(
            "TaskGet",
            json!({"id":id,"block":true,"timeout":1_000}),
            root.path(),
            &session,
        )
        .await
        .unwrap();
        assert_eq!(cancelled["retrieval_status"], "success");
        assert_eq!(cancelled["status"], "cancelled");
    }

    #[tokio::test]
    async fn background_job_task_get_deadline_bounds_a_locked_report_database() {
        use std::time::{Duration, SystemTime, UNIX_EPOCH};
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let id = uuid::Uuid::new_v4().to_string();
        let dir = root.path().join("background-tasks").join(&id);
        std::fs::create_dir_all(&dir).unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let job = json!({"id":id,"foreground":false,"parent_session_id":session,"session_id":uuid::Uuid::new_v4().to_string(),"title":"Review changes","status":"completed","supervisor_pid":null,"child_pid":null,"started_ms":now,"heartbeat_ms":now,"finished_ms":now,"exit_code":0,"error":null});
        std::fs::write(dir.join("state.json"), job.to_string()).unwrap();
        std::fs::write(
            dir.join("output.jsonl"),
            "{\"type\":\"result\",\"result\":\"available report\"}\n",
        )
        .unwrap();
        // A writer holding the shared database must not hold the retrieval
        // open beyond the wait deadline plus a short bounded read. The job's
        // own status is still reported; only the supplementary report remains
        // unavailable.
        let lock =
            rusqlite::Connection::open(root.path().join(crate::runtime::DATABASE_FILE)).unwrap();
        lock.execute_batch("BEGIN EXCLUSIVE").unwrap();
        let started = tokio::time::Instant::now();
        let value = execute_for_session_async(
            "TaskGet",
            json!({"task_id":id,"block":true,"timeout":400}),
            root.path(),
            &session,
        )
        .await
        .unwrap();
        let elapsed = started.elapsed();
        lock.execute_batch("ROLLBACK").unwrap();
        assert!(elapsed >= Duration::from_millis(400), "{elapsed:?}");
        assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
        assert_eq!(value["status"], "completed");
        assert_eq!(value["retrieval_status"], "timeout");
    }

    #[tokio::test]
    async fn background_job_task_get_timeout_zero_read_is_bounded() {
        use std::time::{Duration, SystemTime, UNIX_EPOCH};
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        let id = uuid::Uuid::new_v4().to_string();
        let dir = root.path().join("background-tasks").join(&id);
        std::fs::create_dir_all(&dir).unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let job = json!({"id":id,"foreground":false,"parent_session_id":session,"session_id":uuid::Uuid::new_v4().to_string(),"title":"Review changes","status":"running","supervisor_pid":null,"child_pid":null,"started_ms":now,"heartbeat_ms":now,"finished_ms":null,"exit_code":null,"error":null});
        std::fs::write(dir.join("state.json"), job.to_string()).unwrap();
        let lock =
            rusqlite::Connection::open(root.path().join(crate::runtime::DATABASE_FILE)).unwrap();
        lock.execute_batch("BEGIN EXCLUSIVE").unwrap();
        // timeout:0 still reads the snapshot, bounded by a short read deadline.
        let started = tokio::time::Instant::now();
        let value = execute_for_session_async(
            "TaskGet",
            json!({"task_id":id,"block":true,"timeout":0}),
            root.path(),
            &session,
        )
        .await
        .unwrap();
        let elapsed = started.elapsed();
        lock.execute_batch("ROLLBACK").unwrap();
        assert!(elapsed < Duration::from_secs(3), "{elapsed:?}");
        assert_eq!(value["status"], "running");
        assert_eq!(value["retrieval_status"], "timeout");
    }

    fn execute(name: &str, input: Value, cwd: &str) -> Result<Value> {
        execute_at(name, input, &Path::new(cwd).join("test-data/tasks.sqlite3"))
    }

    #[test]
    fn task_display_title_is_scoped_and_does_not_create_storage() {
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        assert!(display_title(root.path(), &session, &json!({"id":"1"})).is_none());
        assert!(!root.path().join("tasks").exists());
        let task = execute_for_session(
            "TaskCreate",
            json!({"subject":"Build\n项目"}),
            root.path(),
            &session,
        )
        .unwrap();
        for key in ["id", "task_id", "taskId", "shell_id"] {
            let input = json!({key: task["id"]});
            assert_eq!(
                display_title(root.path(), &session, &input).as_deref(),
                Some("Build项目")
            );
            assert!(
                display_title(root.path(), &uuid::Uuid::new_v4().to_string(), &input).is_none()
            );
        }
        assert!(display_title(root.path(), "../invalid", &json!({"id":"1"})).is_none());
    }

    #[test]
    fn sessions_are_isolated_and_legacy_is_read_only() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        let project = temp.path().join("repo");
        std::fs::create_dir_all(&project).unwrap();
        let a = uuid::Uuid::new_v4().to_string();
        let b = uuid::Uuid::new_v4().to_string();
        let todos = json!({"todos":[{"content":"session a", "status":"pending"}]});
        execute_for_session("TodoWrite", todos.clone(), &root, &a).unwrap();
        assert_eq!(
            execute_for_session("TodoRead", json!({}), &root, &a).unwrap(),
            todos
        );
        assert_eq!(
            execute_for_session("TodoRead", json!({}), &root, &b).unwrap()["todos"],
            json!([])
        );
        assert!(!project.join(".claude").exists());
        assert!(execute_for_session("TodoRead", json!({}), &root, "../escape").is_err());
        let legacy = project.join(".claude/tasks.sqlite3");
        execute_at("TodoWrite", todos.clone(), &legacy).unwrap();
        let bytes = std::fs::read(&legacy).unwrap();
        assert_eq!(
            read_legacy("TodoRead", json!({}), project.to_str().unwrap()).unwrap(),
            todos
        );
        assert!(read_legacy("TodoWrite", json!({"todos":[]}), project.to_str().unwrap()).is_err());
        assert_eq!(std::fs::read(legacy).unwrap(), bytes);
    }

    #[test]
    fn legacy_task_and_todo_parameters_remain_usable() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        let task = execute(
            "TaskCreate",
            json!({"name":"legacy","priority":"critical"}),
            cwd,
        )
        .unwrap();
        execute(
            "TaskUpdate",
            json!({"id":task["id"],"status":"running","output":"actual output"}),
            cwd,
        )
        .unwrap();
        assert_eq!(
            execute("TaskOutput", json!({"id":task["id"]}), cwd).unwrap()["output"],
            "actual output"
        );
        assert_eq!(
            execute("TaskStop", json!({"shell_id":task["id"]}), cwd).unwrap()
                ["worker_process_stopped"],
            false
        );
        assert_eq!(
            execute("TodoRead", json!({}), cwd).unwrap()["todos"],
            json!([])
        );
    }
    #[test]
    fn tasks_persist_with_dependencies_and_failed_updates_roll_back() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        let a = execute(
            "TaskCreate",
            json!({"subject":"first","description":"one"}),
            cwd,
        )
        .unwrap();
        let b = execute(
            "TaskCreate",
            json!({"subject":"second","description":"two"}),
            cwd,
        )
        .unwrap();
        execute(
            "TaskUpdate",
            json!({"taskId":b["id"],"addBlockedBy":[a["id"]]}),
            cwd,
        )
        .unwrap();
        assert!(execute(
            "TaskUpdate",
            json!({"taskId":a["id"],"subject":"must roll back","addBlockedBy":[b["id"]]}),
            cwd
        )
        .is_err());
        let actual = execute("TaskGet", json!({"taskId":a["id"]}), cwd).unwrap();
        assert_eq!(actual["subject"], "first");
        assert_eq!(actual["blocks"], json!([b["id"]]));
        execute(
            "TaskUpdate",
            json!({"taskId":a["id"],"status":"deleted"}),
            cwd,
        )
        .unwrap();
        let actual = execute("TaskGet", json!({"taskId":b["id"]}), cwd).unwrap();
        assert_eq!(actual["blockedBy"], json!([]));
        assert_eq!(
            execute("TaskList", json!({}), cwd).unwrap()["tasks"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }
    #[test]
    fn invalid_todos_do_not_replace_saved_list() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        let good = json!({"todos":[{"content":"task","status":"pending"}]});
        execute("TodoWrite", good.clone(), cwd).unwrap();
        assert!(execute(
            "TodoWrite",
            json!({"todos":[{"content":"bad","status":"made_up"}]}),
            cwd
        )
        .is_err());
        assert_eq!(execute("TodoRead", json!({}), cwd).unwrap(), good);
    }
}
