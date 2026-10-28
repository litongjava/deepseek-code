//! Durable, session-local notification claims for managed shell completion.
use anyhow::Result;
use std::path::Path;

/// A managed shell that finished since the last collection.
#[derive(Debug, Clone, PartialEq)]
pub struct Finished {
    pub id: String,
    pub subject: String,
    /// completed, failed or cancelled.
    pub status: String,
    pub exit_code: Option<i64>,
    pub output_file: std::path::PathBuf,
}

impl Finished {
    /// The line shown when it finishes, as in the reference CLI:
    /// `Background command "build" completed (exit code 0)`.
    pub fn summary(&self) -> String {
        let code = self.exit_code.map(|code| format!(" (exit code {code})")).unwrap_or_default();
        match self.status.as_str() {
            "cancelled" => format!("Background command \"{}\" was stopped", self.subject),
            status => format!("Background command \"{}\" {status}{code}", self.subject),
        }
    }

    /// What the model reads, in the reference CLI's notification format.
    pub fn notification(&self) -> String {
        format!(
            "{NOTIFICATION_START}\n<task-id>{}</task-id>\n<output-file>{}</output-file>\n<status>{}</status>\n<summary>{}</summary>\n</task-notification>",
            self.id,
            self.output_file.display(),
            self.status,
            self.summary()
        )
    }
}

/// Every task notification starts with this tag.
pub const NOTIFICATION_START: &str = "<task-notification>";

/// The `<summary>` of a task notification, for display.
pub fn notification_summary(text: &str) -> Option<&str> {
    let rest = text.strip_prefix(NOTIFICATION_START)?;
    let start = rest.find("<summary>")? + "<summary>".len();
    let end = rest[start..].find("</summary>")? + start;
    Some(&rest[start..end])
}

/// Claims every managed shell of the session that finished since the last
/// call; each is returned exactly once, across processes.
pub fn collect_finished(root: &Path, session: &str) -> Result<Vec<Finished>> {
    let session = uuid::Uuid::parse_str(session)?.to_string();
    let path = root.join("tasks").join(format!("{session}.sqlite3"));
    if !path.exists() {
        return Ok(vec![]);
    }
    let mut connection = rusqlite::Connection::open(path)?;
    connection.busy_timeout(std::time::Duration::from_millis(100))?;
    let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS shell_notification_claims(task_id TEXT PRIMARY KEY)",
    )?;
    let tasks = {
        let mut statement = tx.prepare("SELECT tasks.id, json_extract(body,'$.subject'), json_extract(body,'$.status'), json_extract(body,'$.output'), json_extract(body,'$.metadata.log_dir') FROM tasks WHERE NOT EXISTS (SELECT 1 FROM shell_notification_claims c WHERE c.task_id=tasks.id) AND json_extract(body,'$.metadata.managed_shell')=1 AND json_extract(body,'$.status') IN ('completed','failed','cancelled') ORDER BY tasks.rowid")?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>()?
    };
    let mut finished = Vec::new();
    for (id, subject, status, output, log_dir) in tasks {
        let id = uuid::Uuid::parse_str(&id)?.to_string();
        let base = root.join("shell-tasks").join(&session);
        if !base.join(&id).is_dir() {
            continue;
        }
        if tx.execute(
            "INSERT OR IGNORE INTO shell_notification_claims(task_id) VALUES(?1)",
            [&id],
        )? == 0
        {
            continue;
        }
        let subject: String = subject
            .as_deref()
            .unwrap_or("Background shell")
            .chars()
            .filter(|ch| !ch.is_control() && !matches!(ch, '<' | '>' | '"'))
            .take(160)
            .collect();
        let exit_code = output
            .and_then(|output| serde_json::from_str::<serde_json::Value>(&output).ok())
            .and_then(|output| output["exit_code"].as_i64());
        let directory = match log_dir.filter(|name| name.starts_with("fg-") && uuid::Uuid::parse_str(&name[3..]).is_ok()) {
            Some(name) => base.join(name),
            None => base.join(&id),
        };
        finished.push(Finished { id, subject, status, exit_code, output_file: directory.join("stdout.log") });
    }
    tx.commit()?;
    Ok(finished)
}

pub fn collect_completed(root: &Path, session: &str) -> Result<Vec<String>> {
    Ok(collect_finished(root, session)?
        .into_iter()
        .map(|task| format!("Background shell {}: {}\n  /tasks show {}", task.status, task.subject, task.id))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn notifications_follow_the_reference_format() {
        let task = Finished { id: "t1".into(), subject: "Wait then print".into(), status: "completed".into(), exit_code: Some(0), output_file: "/d/stdout.log".into() };
        assert_eq!(task.summary(), "Background command \"Wait then print\" completed (exit code 0)");
        let text = task.notification();
        assert!(text.starts_with(NOTIFICATION_START) && text.contains("<task-id>t1</task-id>") && text.contains("<status>completed</status>"));
        assert_eq!(notification_summary(&text), Some(task.summary().as_str()));
        assert_eq!(notification_summary("plain text"), None);
        let stopped = Finished { status: "cancelled".into(), exit_code: None, ..task };
        assert_eq!(stopped.summary(), "Background command \"Wait then print\" was stopped");
    }
    #[test]
    fn completion_claim_is_durable_concurrent_and_session_local() {
        let root = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::new_v4().to_string();
        for status in ["completed", "failed", "cancelled", "running"] {
            let task = crate::tools::tasks::execute_for_shell_owner(
                "TaskCreate",
                serde_json::json!({"subject":status,"metadata":{"managed_shell":true}}),
                root.path(),
                &session,
            )
            .unwrap();
            let id = task["id"].as_str().unwrap();
            std::fs::create_dir_all(root.path().join("shell-tasks").join(&session).join(id))
                .unwrap();
            crate::tools::tasks::execute_for_shell_owner(
                "TaskUpdate",
                serde_json::json!({"id":id,"status":status}),
                root.path(),
                &session,
            )
            .unwrap();
        }
        assert!(
            collect_completed(root.path(), &uuid::Uuid::new_v4().to_string())
                .unwrap()
                .is_empty()
        );
        let notices = std::thread::scope(|scope| {
            let one = scope.spawn(|| collect_completed(root.path(), &session).unwrap());
            let two = scope.spawn(|| collect_completed(root.path(), &session).unwrap());
            [one.join().unwrap(), two.join().unwrap()].concat()
        });
        assert_eq!(notices.len(), 3);
        for status in ["completed", "failed", "cancelled"] {
            assert_eq!(
                notices
                    .iter()
                    .filter(|notice| notice.starts_with(&format!("Background shell {status}:")))
                    .count(),
                1
            );
        }
        assert!(collect_completed(root.path(), &session).unwrap().is_empty());
    }
}
