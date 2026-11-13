//! Durable discovery for idle local sessions. Parking never submits a model turn.
use super::{background_sessions as jobs, session_permission_mode, session_worker, ServiceContext};
use crate::{permissions::PermissionMode, runtime::SessionStore};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ParkedSession {
    pub parent_session_id: String,
    pub session_id: String,
    pub initial_worker_id: String,
    pub created_at: String,
}
#[derive(Debug, Serialize)]
pub struct ParkedStatus {
    pub session: ParkedSession,
    /// Actual worker observation, independent of the durable session record.
    pub worker_state: String,
    pub worker_pid: Option<u32>,
    pub activity: String,
    pub active_job: Option<String>,
    pub model: Option<String>,
    pub provider: Option<String>,
    pub resume_permission_mode: String,
}

fn prefix(parent: &str) -> Result<String> {
    ensure!(
        uuid::Uuid::parse_str(parent).is_ok(),
        "Invalid parent session ID"
    );
    Ok(format!("parked-session:{parent}:"))
}
fn key(parent: &str, session: &str) -> Result<String> {
    ensure!(
        uuid::Uuid::parse_str(session).is_ok(),
        "Invalid parked session ID"
    );
    Ok(format!("{}{session}", prefix(parent)?))
}
fn active(context: &ServiceContext, session: &str) -> Result<Option<jobs::Job>> {
    Ok(jobs::list(&context.paths.data_dir, None)?
        .into_iter()
        .find(|job| {
            job.session_id == session
                && matches!(job.status.as_str(), "starting" | "running" | "unresponsive")
        }))
}

/// Validate the same project trust/worktree binding used by normal resume.
pub fn validate_idle(context: &ServiceContext, parent: &str, session: &str) -> Result<PathBuf> {
    key(parent, session)?;
    let store = SessionStore::new(context.db.clone());
    store.resume(parent)?;
    store.resume(session)?;
    if let Some(job) = active(context, session)? {
        anyhow::bail!("Session has active task {}; use /tasks attach {} or cancel it before parking an idle session", job.id, job.id);
    }
    store.workspace(session, &context.paths.data_dir.join("workspaces"))
}

pub async fn park(
    context: &ServiceContext,
    parent: &str,
    session: &str,
    executable: &Path,
) -> Result<ParkedSession> {
    let workspace = validate_idle(context, parent, session)?;
    let worker =
        session_worker::ensure_worker(&context.paths.data_dir, executable, session, &workspace)
            .await?;
    // Another frontend may submit while the IPC startup acknowledgement is in flight.
    validate_idle(context, parent, session)?;
    let record = ParkedSession {
        parent_session_id: parent.into(),
        session_id: session.into(),
        initial_worker_id: worker.id,
        created_at: chrono::Utc::now().to_rfc3339(),
    };
    context.save_setting(&key(parent, session)?, &record)
        .with_context(||format!("Idle worker started for session {session}, but discovery registration failed; resume that session directly"))?;
    Ok(record)
}

pub fn registered(context: &ServiceContext, parent: &str) -> Result<Vec<ParkedSession>> {
    let prefix = prefix(parent)?;
    context.db.with_conn(|conn| -> Result<_> {
        let mut query =
            conn.prepare("SELECT value FROM peripheral_settings WHERE key LIKE ?1 ORDER BY key")?;
        let rows = query.query_map([format!("{prefix}%")], |row| row.get::<_, String>(0))?;
        let mut records = Vec::new();
        for row in rows {
            let record: ParkedSession = serde_json::from_str(&row?)?;
            ensure!(
                record.parent_session_id == parent,
                "Parked session ownership mismatch"
            );
            key(parent, &record.session_id)?;
            records.push(record);
        }
        Ok(records)
    })?
}

pub async fn list(context: &ServiceContext, parent: &str) -> Result<Vec<ParkedStatus>> {
    let records = registered(context, parent)?;
    futures::future::try_join_all(records.into_iter().map(|record| async move {
        let worker =
            session_worker::runtime_status(&context.paths.data_dir, &record.session_id).await;
        let job = active(context, &record.session_id)?;
        let saved = context.db.with_conn(|conn| {
            crate::db::conversation_repo::get_conversation(conn, &record.session_id)
        })??;
        let mode = session_permission_mode::load(&context.db, &record.session_id)?;
        let mode = match mode {
            Some(PermissionMode::BypassPermissions) | None => PermissionMode::AskPermissions,
            Some(mode) => mode,
        };
        let activity = if job.is_some() {
            "active_task"
        } else if saved.as_ref().is_none_or(|row| row.archived) {
            "session_unavailable"
        } else if worker.state == "reachable" {
            "waiting_for_input"
        } else {
            "session_saved_worker_unavailable"
        };
        Ok(ParkedStatus {
            session: record,
            worker_state: worker.state,
            worker_pid: worker.pid,
            activity: activity.into(),
            active_job: job.map(|job| job.id),
            model: saved.as_ref().and_then(|row| row.model.clone()),
            provider: saved.and_then(|row| row.provider),
            resume_permission_mode: session_permission_mode::name(mode).into(),
        })
    }))
    .await
}

pub async fn stop(context: &ServiceContext, parent: &str, session: &str) -> Result<()> {
    ensure!(
        parent == session
            || registered(context, parent)?
                .iter()
                .any(|record| record.session_id == session),
        "Background session belongs to another parent"
    );
    key(parent, session)?;
    if let Some(job) = active(context, session)? {
        anyhow::bail!(
            "Session has active task {}; cancel the task explicitly before stopping its worker",
            job.id
        );
    }
    session_worker::shutdown(&context.paths.data_dir, session).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    fn fixture() -> (tempfile::TempDir, ServiceContext, String, String) {
        let temp = tempfile::tempdir().unwrap();
        let context = ServiceContext::open(
            crate::runtime::RuntimePaths::new(temp.path().join("data")).unwrap(),
            Arc::new(super::super::EnvironmentCredentials),
        )
        .unwrap();
        let store = SessionStore::new(context.db.clone());
        let parent = store.create(temp.path(), "model", "provider").unwrap();
        store.append(&parent, "user", "Original task").unwrap();
        let child = store.fork(&parent).unwrap();
        (temp, context, parent, child)
    }
    #[tokio::test]
    async fn saved_sessions_are_not_reported_as_live_workers_and_ownership_is_enforced() {
        let (_temp, context, parent, child) = fixture();
        let record = ParkedSession {
            parent_session_id: parent.clone(),
            session_id: child.clone(),
            initial_worker_id: uuid::Uuid::new_v4().to_string(),
            created_at: "now".into(),
        };
        context
            .save_setting(&key(&parent, &child).unwrap(), &record)
            .unwrap();
        context
            .db
            .with_conn(|conn| {
                conn.execute(
                    "INSERT INTO peripheral_settings(key,value) VALUES(?1,'bypass')",
                    [format!("cli.permission-mode.{child}")],
                )
            })
            .unwrap()
            .unwrap();
        let report = list(&context, &parent).await.unwrap();
        assert_eq!(report.len(), 1);
        assert_eq!(report[0].worker_state, "not_started");
        assert_eq!(report[0].activity, "session_saved_worker_unavailable");
        assert_eq!(report[0].model.as_deref(), Some("model"));
        assert_eq!(report[0].resume_permission_mode, "ask");
        assert!(registered(&context, &child).unwrap().is_empty());
        assert!(stop(&context, &child, &parent).await.is_err());
        assert!(jobs::list(&context.paths.data_dir, None)
            .unwrap()
            .is_empty());
    }
    #[test]
    fn untrusted_workspace_cannot_start_an_idle_worker() {
        let (_temp, context, parent, child) = fixture();
        let project = super::super::session_trust::inspect(&context.db, &child).unwrap();
        context
            .db
            .with_conn(|conn| {
                conn.execute(
                    "UPDATE project_folders SET trusted=0 WHERE project_id=?1",
                    [&project.id],
                )
            })
            .unwrap()
            .unwrap();
        assert!(validate_idle(&context, &parent, &child).is_err());
    }
    #[test]
    fn parked_session_validation_preserves_trust_and_rejects_active_tasks() {
        let (_temp, context, parent, child) = fixture();
        assert!(validate_idle(&context, &parent, &child).is_ok());
        let id = uuid::Uuid::new_v4().to_string();
        let directory = context.paths.data_dir.join("background-tasks").join(&id);
        std::fs::create_dir_all(&directory).unwrap();
        let job = jobs::Job {
            id: id.clone(),
            foreground: false,
            parent_session_id: parent.clone(),
            session_id: child.clone(),
            title: "active".into(),
            status: "running".into(),
            supervisor_pid: Some(std::process::id()),
            child_pid: Some(std::process::id()),
            started_ms: jobs::now(),
            heartbeat_ms: jobs::now(),
            finished_ms: None,
            exit_code: None,
            error: None,
            launched: true,
        };
        jobs::save(&directory.join("state.json"), &job).unwrap();
        assert!(validate_idle(&context, &parent, &child)
            .unwrap_err()
            .to_string()
            .contains(&id));
        assert!(key(&parent, "../escape").is_err());
    }
}
