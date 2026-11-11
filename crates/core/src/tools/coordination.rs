//! Durable teams; execution is delegated by the host to its existing tool loop.
//!
//! The duplicate model-facing project-plan entry (`plan_mode`) is retired. Its
//! legacy records are read only by `tools::plan_import` and are never deleted.
use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension};
use serde_json::{json, Value};

static TEAM_LOCKS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, TeamLock>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

/// Where a team's execution lock lives.
fn team_lock_path(cwd: &str, id: &str) -> Result<std::path::PathBuf> {
    Ok(std::path::Path::new(cwd)
        .canonicalize()?
        .join(".claude")
        .join(format!("team-{id}.lock")))
}

/// Owns a team's execution lock until its scope ends.
///
/// Closing the descriptor is not enough to release ownership: `fork` gives the
/// child the same open file description, so a child spawned while the team was
/// running keeps the lock held after the owner drops its `File`. Explicit
/// unlock clears the lock on that shared description, so the next coordinator
/// does not wait for an unrelated process to exit.
struct TeamLock {
    file: std::fs::File,
    released: bool,
}

impl TeamLock {
    fn new(file: std::fs::File) -> Self {
        Self {
            file,
            released: false,
        }
    }

    /// Releases ownership now instead of leaving it to the descriptor close.
    fn release(&mut self) {
        if !self.released {
            let _ = crate::fs::unlock_file(&self.file);
            self.released = true;
        }
    }
}

impl Drop for TeamLock {
    fn drop(&mut self) {
        self.release();
    }
}

fn team_lock(cwd: &str, id: &str) -> Result<(String, TeamLock)> {
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-') {
        bail!("Invalid team ID");
    }
    let path = team_lock_path(cwd, id)?;
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)?;
    crate::fs::try_lock_file(&file)
        .context("Team execution is active in another request or process")?;
    Ok((path.to_string_lossy().into_owned(), TeamLock::new(file)))
}

pub(super) fn definitions() -> Vec<super::ToolDefinition> {
    vec![
        super::ToolDefinition{name:"team".into(),description:"Manage persistent project agent teams: create, get, list, delete, reset_task. Use run to execute pending tasks through the host's shared agent engine. Creation does not execute tasks.".into(),input_schema:json!({"type":"object","properties":{"operation":{"type":"string","enum":["create","get","list","delete","run","reset_task"]},"team_id":{"type":"string"},"task_id":{"type":"string"},"name":{"type":"string"},"tasks":{"type":"array","items":{"type":"object","properties":{"prompt":{"type":"string"},"description":{"type":"string"}},"required":["prompt"]}}},"required":["operation"]})}
    ]
}

fn connection(cwd: &str) -> Result<rusqlite::Connection> {
    let directory = std::path::Path::new(cwd).canonicalize()?.join(".claude");
    super::resolve_path(
        &directory.join("coordination.sqlite3").to_string_lossy(),
        cwd,
    )?;
    std::fs::create_dir_all(&directory)?;
    let db = rusqlite::Connection::open(directory.join("coordination.sqlite3"))?;
    db.busy_timeout(std::time::Duration::from_secs(5))?;
    db.execute_batch("CREATE TABLE IF NOT EXISTS records(kind TEXT NOT NULL,id TEXT NOT NULL,body TEXT NOT NULL,PRIMARY KEY(kind,id)); CREATE TABLE IF NOT EXISTS current_plan(singleton INTEGER PRIMARY KEY CHECK(singleton=1),id TEXT NOT NULL);")?;
    Ok(db)
}
fn get(db: &rusqlite::Connection, kind: &str, id: &str) -> Result<Value> {
    let body: Option<String> = db
        .query_row(
            "SELECT body FROM records WHERE kind=?1 AND id=?2",
            params![kind, id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(serde_json::from_str(
        &body.with_context(|| format!("{kind} not found: {id}"))?,
    )?)
}
fn put(db: &rusqlite::Connection, kind: &str, id: &str, body: &Value) -> Result<()> {
    db.execute("INSERT INTO records(kind,id,body) VALUES(?1,?2,?3) ON CONFLICT(kind,id) DO UPDATE SET body=excluded.body",params![kind,id,body.to_string()])?;
    Ok(())
}
fn required<'a>(input: &'a Value, key: &str) -> Result<&'a str> {
    input[key]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .with_context(|| format!("{key} is required"))
}

pub fn execute(name: &str, input: Value, cwd: &str) -> Result<Value> {
    // The duplicate project-plan entry (`plan_mode`) is retired. Refusing an
    // unknown name before opening the database is what makes a stale settings
    // entry or an old transcript unable to create legacy storage.
    if name != "team" {
        bail!("Unknown coordination tool");
    }
    let mut db = connection(cwd)?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let operation = required(&input, "operation")?;
    let kind = "team";
    let id_key = "team_id";
    let result = match operation {
        "create" => {
            let id = uuid::Uuid::new_v4().to_string();
            let timestamp = chrono::Utc::now().to_rfc3339();
            let name = required(&input, "name")?;
            let tasks = input["tasks"].as_array().context("tasks is required")?;
            if tasks.is_empty() || tasks.len() > 100 {
                bail!("Team needs 1–100 tasks");
            }
            let tasks=tasks.iter().enumerate().map(|(index,task)|Ok(json!({"task_id":format!("task-{index}"),"prompt":required(task,"prompt")?,"description":task["description"].as_str().unwrap_or(""),"status":"pending","result":null,"error":null}))).collect::<Result<Vec<_>>>()?;
            let mut record = json!({"team_id":id,"name":name,"tasks":tasks,"status":"active"});
            record["created_at"] = json!(timestamp);
            record["updated_at"] = json!(timestamp);
            put(&tx, kind, &id, &record)?;
            record
        }
        "list" => {
            let mut statement =
                tx.prepare("SELECT body FROM records WHERE kind=?1 ORDER BY rowid")?;
            let rows = statement.query_map([kind], |row| row.get::<_, String>(0))?;
            let mut records = vec![];
            for row in rows {
                records.push(serde_json::from_str::<Value>(&row?)?);
            }
            json!({"items":records})
        }
        "get" => get(&tx, kind, required(&input, id_key)?)?,
        "delete" if kind == "team" => {
            let id = required(&input, id_key)?;
            let team = get(&tx, kind, id)?;
            if team["tasks"]
                .as_array()
                .unwrap()
                .iter()
                .any(|task| task["status"] == "running")
            {
                bail!("Cannot delete a team with running tasks; cancel execution first");
            }
            tx.execute(
                "DELETE FROM records WHERE kind=?1 AND id=?2",
                params![kind, id],
            )?;
            json!({"team_id":id,"deleted":true})
        }
        "reset_task" if kind == "team" => {
            let id = required(&input, id_key)?;
            let mut team = get(&tx, kind, id)?;
            let task_id = required(&input, "task_id")?;
            let task = team["tasks"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|task| task["task_id"] == task_id)
                .context("Task not found")?;
            if task["status"] == "running" {
                // A crashed host releases its OS lock; a live host cannot be reset.
                let _lock = team_lock(cwd, id)?;
            }
            task["status"] = json!("pending");
            task["result"] = Value::Null;
            task["error"] = Value::Null;
            team["status"] = json!("active");
            put(&tx, kind, id, &team)?;
            team
        }
        "run" if kind == "team" => bail!("Team execution requires the shared agent host"),
        _ => bail!("Unsupported {kind} operation: {operation}"),
    };
    tx.commit()?;
    Ok(result)
}

pub fn claim_team_task(cwd: &str, team_id: &str) -> Result<Option<Value>> {
    let mut db = connection(cwd)?;
    let (lock_key, lock) = team_lock(cwd, team_id)?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let mut team = get(&tx, "team", team_id)?;
    let tasks = team["tasks"].as_array_mut().context("Invalid team tasks")?;
    // One coordinator owns a team at a time; duplicate run calls cannot split tasks.
    for task in tasks.iter_mut().filter(|task| task["status"] == "running") {
        task["status"] = json!("failed");
        task["error"] = json!("Previous execution host exited; reset_task explicitly to retry");
    }
    let task = tasks.iter_mut().find(|task| task["status"] == "pending");
    let result = task.map(|task| {
        task["status"] = json!("running");
        task["host_pid"] = json!(std::process::id());
        task.clone()
    });
    if result.is_none() {
        team["status"] = json!(if tasks.iter().any(|task| task["status"] == "failed") {
            "failed"
        } else {
            "completed"
        });
    }
    put(&tx, "team", team_id, &team)?;
    tx.commit()?;
    if result.is_some() {
        TEAM_LOCKS
            .lock()
            .map_err(|_| anyhow::anyhow!("Team lock registry is poisoned"))?
            .insert(lock_key, lock);
    }
    Ok(result)
}

pub fn finish_team_task(
    cwd: &str,
    team_id: &str,
    task_id: &str,
    result: Option<&str>,
    error: Option<&str>,
) -> Result<()> {
    let mut db = connection(cwd)?;
    let tx = db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let mut team = get(&tx, "team", team_id)?;
    let tasks = team["tasks"].as_array_mut().context("Invalid team tasks")?;
    let task = tasks
        .iter_mut()
        .find(|task| task["task_id"] == task_id)
        .context("Task not found")?;
    if task["status"] != "running" {
        bail!("Task is not running");
    }
    task["status"] = json!(if error.is_some() {
        "failed"
    } else {
        "completed"
    });
    task["result"] = json!(result);
    task["error"] = json!(error);
    let status = if tasks.iter().any(|task| task["status"] == "failed") {
        "failed"
    } else if tasks.iter().all(|task| task["status"] == "completed") {
        "completed"
    } else {
        "active"
    };
    team["status"] = json!(status);
    team["updated_at"] = json!(chrono::Utc::now().to_rfc3339());
    put(&tx, "team", team_id, &team)?;
    tx.commit()?;
    let lock_key = team_lock_path(cwd, team_id)?.to_string_lossy().into_owned();
    let released = TEAM_LOCKS
        .lock()
        .map_err(|_| anyhow::anyhow!("Team lock registry is poisoned"))?
        .remove(&lock_key);
    if let Some(mut lock) = released {
        lock.release();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn teams_claim_once_and_persist_real_results() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        let team=execute("team",json!({"operation":"create","name":"review","tasks":[{"prompt":"inspect"},{"prompt":"verify"}]}),cwd).unwrap();
        let id = team["team_id"].as_str().unwrap();
        let task = claim_team_task(cwd, id).unwrap().unwrap();
        assert!(claim_team_task(cwd, id).is_err());
        assert!(execute("team", json!({"operation":"delete","team_id":id}), cwd).is_err());
        finish_team_task(
            cwd,
            id,
            task["task_id"].as_str().unwrap(),
            Some("inspected"),
            None,
        )
        .unwrap();
        let task = claim_team_task(cwd, id).unwrap().unwrap();
        finish_team_task(
            cwd,
            id,
            task["task_id"].as_str().unwrap(),
            None,
            Some("cancelled"),
        )
        .unwrap();
        let team = execute("team", json!({"operation":"get","team_id":id}), cwd).unwrap();
        assert_eq!(team["status"], "failed");
        assert_eq!(team["tasks"][0]["result"], "inspected");
    }
    #[test]
    fn releasing_team_ownership_survives_an_inherited_descriptor() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        let team=execute("team",json!({"operation":"create","name":"review","tasks":[{"prompt":"inspect"},{"prompt":"verify"}]}),cwd).unwrap();
        let id = team["team_id"].as_str().unwrap().to_owned();
        let task = claim_team_task(cwd, &id).unwrap().unwrap();
        // Duplicate the descriptor that owns the flock. Both descriptors share
        // one open file description, exactly like a forked child's inherited
        // copy, so closing the owner alone leaves the lock held. This is a
        // deterministic shared-description proof, not a fork/exec execution.
        let key = team_lock_path(cwd, &id)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let duplicate = {
            let locks = TEAM_LOCKS.lock().unwrap();
            locks
                .get(&key)
                .expect("the claim owns the team lock")
                .file
                .try_clone()
                .unwrap()
        };
        assert!(
            claim_team_task(cwd, &id).is_err(),
            "duplicate active claims must still be denied"
        );

        finish_team_task(
            cwd,
            &id,
            task["task_id"].as_str().unwrap(),
            Some("inspected"),
            None,
        )
        .unwrap();

        // The duplicate descriptor is still open here: releasing ownership must
        // not depend on it closing.
        let second = claim_team_task(cwd, &id)
            .unwrap()
            .expect("the second claim must not stay blocked behind an inherited descriptor");
        finish_team_task(
            cwd,
            &id,
            second["task_id"].as_str().unwrap(),
            None,
            Some("cancelled"),
        )
        .unwrap();
        drop(duplicate);

        let team = execute("team", json!({"operation":"get","team_id":&id}), cwd).unwrap();
        assert_eq!(team["status"], "failed");
        assert_eq!(team["tasks"][0]["result"], "inspected");
    }
    #[test]
    fn retired_plan_entry_fails_before_creating_legacy_storage() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        for operation in ["create", "enter", "exit", "get_current", "list"] {
            let error = execute(
                "plan_mode",
                json!({"operation":operation,"task":"build","steps":[]}),
                cwd,
            )
            .unwrap_err();
            assert!(
                error.to_string().contains("Unknown coordination tool"),
                "{operation}: {error}"
            );
        }
        assert!(!dir.path().join(".claude").exists());
    }
}
