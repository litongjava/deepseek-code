//! Session-scoped application-data task records. No background worker completion is fabricated.
use anyhow::{bail, ensure, Context, Result};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};
use std::path::Path;

pub(super) fn definitions() -> Vec<super::ToolDefinition> {
    [
        ("TaskCreate", "Create a durable session task.", json!({"name":{"type":"string"},"priority":{"type":"string"},"subject":{"type":"string"},"description":{"type":"string"},"activeForm":{"type":"string"},"metadata":{"type":"object"}}), vec![]),
        ("TaskGet", "Read a durable session task by ID.", json!({"id":{"type":"string"},"taskId":{"type":"string"}}),vec![]),
        ("TaskList", "List durable session tasks.", json!({}),vec![]),
        ("TaskUpdate", "Update a durable session task. Dependencies must exist and cannot form cycles.", json!({"id":{"type":"string"},"taskId":{"type":"string"},"name":{"type":"string"},"priority":{"type":"string"},"subject":{"type":"string"},"description":{"type":"string"},"status":{"type":"string","enum":["pending","in_progress","running","completed","failed","cancelled","deleted"]},"output":{"type":"string"},"owner":{"type":"string"},"activeForm":{"type":"string"},"metadata":{"type":"object"},"addBlockedBy":{"type":"array","items":{"type":"string"}},"addBlocks":{"type":"array","items":{"type":"string"}}}),vec![]),
        ("TodoWrite", "Replace the session's durable todo list.", json!({"todos":{"type":"array","items":{"type":"object","properties":{"content":{"type":"string"},"activeForm":{"type":"string"},"status":{"type":"string","enum":["pending","in_progress","completed"]}},"required":["content","status"]}}}),vec!["todos"]),
        ("TodoRead", "Read the session's durable todo list.",json!({}),vec![]),
        ("TaskOutput", "Read managed background shell logs and status. block defaults to true; timeout is milliseconds (default 30000, maximum 600000). Use block:false for an immediate snapshot. Saved non-process task records return their saved output.",json!({"id":{"type":"string"},"task_id":{"type":"string"},"block":{"type":"boolean","default":true},"timeout":{"type":"integer","minimum":0,"maximum":600000,"default":30000}}),vec![]),
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

fn readonly(name: &str, input: &Value) -> bool {
    matches!(name, "TaskGet" | "TaskList" | "TodoRead" | "TaskOutput")
}

pub fn execute_for_session(name: &str, input: Value, root: &Path, session: &str) -> Result<Value> {
    let session = uuid::Uuid::parse_str(session).context("Invalid task session ID")?;
    if let Some(output) = super::background_shell::control(name, &input, root, &session.to_string())? { return Ok(output); }
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
    if let Some(output) = super::background_shell::control_async(name, &input, root, session).await? {
        return Ok(output);
    }
    let (name, root, session) = (name.to_owned(), root.to_owned(), session.to_owned());
    tokio::task::spawn_blocking(move || execute_for_session(&name, input, &root, &session)).await?
}

pub fn read_legacy(name: &str, input: Value, cwd: &str) -> Result<Value> {
    ensure!(readonly(name, &input), "Legacy project tasks are read-only");
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
    if !shell_owner {
        ensure!(
            input.get("metadata").and_then(|m| m.get("managed_shell")).is_none(),
            "managed_shell provenance is reserved for the shell process owner"
        );
    }
    // shell_id is the reference CLI's deprecated TaskStop alias.
    if input.get("id").is_none() { if let Some(id) = input.get("task_id").or_else(|| input.get("shell_id")).cloned() { input["id"] = id; } }
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
    let readonly = readonly(name, &input);
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
        "TaskGet" => get(input["taskId"].as_str().context("taskId is required")?)?,
        "TaskOutput" => {
            let task = get(input["taskId"].as_str().context("id is required")?)?;
            json!({"id":task["id"],"output":task["output"]})
        }
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
    fn execute(name: &str, input: Value, cwd: &str) -> Result<Value> {
        execute_at(name, input, &Path::new(cwd).join("test-data/tasks.sqlite3"))
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
            execute("TaskStop", json!({"shell_id":task["id"]}), cwd).unwrap()["worker_process_stopped"],
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
