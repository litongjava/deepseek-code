//! SQLite persistence for Super Agent tasks, events, approvals, reports and locks.
//!
//! Every mutation that can be retried carries an `operation_id`. The operation
//! table makes those identifiers globally idempotent: replaying the same request
//! returns the original result, while reusing an identifier for a different
//! task, kind or payload is an explicit conflict.
use super::model::{
    AgentStatus, Approval, ApprovalStatus, ApprovalType, CleanupStatus, Event, EventType,
    IntegrationStatus, NewEvent, NonGitStatus, NonGitTarget, Phase, RepoTarget, Report, ReportKind,
    SessionRef, Status, TaskRecord, TaskSpec, VerificationStatus,
};
use super::state::{self, StateError, StatePolicy};
use crate::db::DbManager;
use anyhow::{ensure, Context, Result as AnyResult};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use thiserror::Error;

const MAX_REPORT_BYTES: usize = 2 * 1024 * 1024;
const MAX_ARTIFACT_BYTES: usize = 64 * 1024 * 1024;
const WRITE_DEADLINE: Duration = Duration::from_secs(10);

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("operation {operation_id} was already used for a different {kind} request")]
    OperationConflict { operation_id: String, kind: String },
    #[error("task not found: {0}")]
    TaskNotFound(String),
    #[error("approval {0} is already resolved")]
    AlreadyResolved(String),
    #[error("lock {0} is held by another task")]
    LockBusy(String),
    #[error("completion gate is not satisfied: {0:?}")]
    Gate(Vec<super::model::Unmet>),
    #[error("invalid super agent request: {0}")]
    Invalid(String),
    #[error(transparent)]
    Transition(#[from] StateError),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Internal(anyhow::Error::new(error))
    }
}

impl From<std::io::Error> for StoreError {
    fn from(error: std::io::Error) -> Self {
        Self::Internal(anyhow::Error::new(error))
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(error: serde_json::Error) -> Self {
        Self::Internal(anyhow::Error::new(error))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct EventAppend {
    pub event: Event,
    pub inserted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockOutcome {
    Acquired,
    AlreadyOwned,
}

#[derive(Debug, Clone)]
pub struct ApprovalDraft {
    pub source_session_id: String,
    pub parent_session_id: String,
    pub request_type: ApprovalType,
    pub request: Value,
    pub task_spec_version: u32,
    pub permission_policy_version: u32,
}

#[derive(Clone)]
pub struct TaskStore {
    db: Arc<DbManager>,
    data_dir: PathBuf,
}

impl TaskStore {
    pub fn new(db: Arc<DbManager>, data_dir: PathBuf) -> Self {
        Self { db, data_dir }
    }

    pub fn from_context(context: &crate::services::ServiceContext) -> Self {
        Self::new(context.db.clone(), context.paths.data_dir.clone())
    }

    pub fn db(&self) -> &Arc<DbManager> {
        &self.db
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    fn run_tx<T>(
        &self,
        label: &str,
        mut operation: impl FnMut(&Transaction<'_>) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let inner = self
            .db
            .with_conn(|connection| {
                crate::db::write_tx(connection, label, WRITE_DEADLINE, |tx| {
                    operation(tx).map_err(anyhow::Error::new)
                })
            })
            .map_err(StoreError::Internal)?
            .map_err(|error| match error.downcast::<StoreError>() {
                Ok(error) => error,
                Err(error) => StoreError::Internal(error),
            })?;
        Ok(inner)
    }

    fn run_read<T>(
        &self,
        read: impl FnOnce(&Connection) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        match self.db.with_conn(read) {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(error),
            Err(error) => Err(StoreError::Internal(error)),
        }
    }

    pub fn create_task(
        &self,
        router_session_id: &str,
        spec: TaskSpec,
        operation_id: &str,
    ) -> Result<TaskRecord, StoreError> {
        validate_operation_id(operation_id)?;
        if router_session_id.trim().is_empty() {
            return Err(StoreError::Invalid("Router session is required".into()));
        }
        spec.validate().map_err(StoreError::Internal)?;
        let hash = hash_json(&json!({"router":router_session_id,"spec":spec}));
        self.run_tx("super-agent create task", |tx| {
            if let Some((kind, task_id, stored)) = operation_row(tx, operation_id)? {
                if kind == "task.create" && stored == hash {
                    return load_record_conn(tx, &task_id)?
                        .ok_or_else(|| StoreError::TaskNotFound(task_id));
                }
                return Err(StoreError::OperationConflict {
                    operation_id: operation_id.into(),
                    kind,
                });
            }
            let number = next_task_number(tx)?;
            let task_id = format!("task-{number:03}");
            let pair_id = format!("crp-{number:03}");
            let now = chrono::Utc::now().to_rfc3339();
            let record = TaskRecord {
                task_id: task_id.clone(),
                pair_id: pair_id.clone(),
                router_session_id: router_session_id.into(),
                title: spec.title.clone(),
                spec: spec.clone(),
                spec_version: 1,
                phase: Phase::Discovering,
                status: Status::Queued,
                review_round: 0,
                parent_task_id: None,
                depends_on: Vec::new(),
                coder: SessionRef::default(),
                reviewer: SessionRef::default(),
                cleanup_status: CleanupStatus::NotStarted,
                permission_policy_version: 1,
                created_at: now.clone(),
                updated_at: now,
                repositories: Vec::new(),
                non_git_targets: Vec::new(),
            };
            insert_task_conn(tx, &record)?;
            claim_operation(tx, operation_id, "task.create", &task_id, &hash)?;
            let event = Event {
                event_id: uuid::Uuid::new_v4().to_string(),
                task_id: task_id.clone(),
                pair_id,
                session_id: Some(router_session_id.into()),
                run_id: None,
                sequence: 1,
                event_type: EventType::TaskCreated,
                causation_id: None,
                operation_id: operation_id.into(),
                timestamp: chrono::Utc::now().to_rfc3339(),
                payload: json!({"spec":spec,"router_session_id":router_session_id}),
                processed: false,
            };
            insert_event_conn(tx, &event)?;
            Ok(record)
        })
    }

    pub fn get_task(&self, task_id: &str) -> Result<Option<TaskRecord>, StoreError> {
        self.run_read(|connection| load_record_conn(connection, task_id))
    }

    pub fn list_tasks(
        &self,
        router_session_id: &str,
        status: Option<Status>,
    ) -> Result<Vec<TaskRecord>, StoreError> {
        self.run_read(|tx| {
            let mut statement = tx.prepare(
                "SELECT task_id FROM sa_tasks WHERE router_session_id=?1 ORDER BY task_id",
            )?;
            let ids = statement
                .query_map([router_session_id], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut records = Vec::new();
            for id in ids {
                if let Some(record) = load_record_conn(tx, &id)? {
                    if status.is_none_or(|wanted| record.status == wanted) {
                        records.push(record);
                    }
                }
            }
            Ok(records)
        })
    }

    pub fn apply_event(
        &self,
        task_id: &str,
        event: NewEvent,
        policy: &StatePolicy,
    ) -> Result<EventAppend, StoreError> {
        validate_operation_id(&event.operation_id)?;
        self.run_tx("super-agent apply event", |tx| {
            if let Some(existing) = event_by_operation(tx, &event.operation_id)? {
                if existing.task_id == task_id
                    && existing.event_type == event.event_type
                    && existing.payload == event.payload
                    && existing.session_id == event.session_id
                    && existing.run_id == event.run_id
                    && existing.causation_id == event.causation_id
                {
                    return Ok(EventAppend {
                        event: existing,
                        inserted: false,
                    });
                }
                return Err(StoreError::OperationConflict {
                    operation_id: event.operation_id.clone(),
                    kind: format!("event:{}", existing.event_type),
                });
            }
            let record = load_record_conn(tx, task_id)?
                .ok_or_else(|| StoreError::TaskNotFound(task_id.into()))?;
            if event.event_type == EventType::TaskCompleted {
                let repos = load_repos_conn(tx, task_id)?;
                let reports = load_reports_conn(tx, task_id)?;
                let approvals = load_approvals_conn(tx, task_id)?;
                if let Err(unmet) =
                    state::completion_gate(&record, &repos, &reports, &approvals)
                {
                    return Err(StoreError::Gate(unmet));
                }
            }
            let transition = state::apply(&record, &event, policy)?;
            let sequence: i64 = tx.query_row(
                "SELECT COALESCE(MAX(sequence),0)+1 FROM sa_events WHERE task_id=?1",
                [task_id],
                |row| row.get(0),
            )?;
            let stored = Event {
                event_id: uuid::Uuid::new_v4().to_string(),
                task_id: task_id.into(),
                pair_id: record.pair_id.clone(),
                session_id: event.session_id.clone(),
                run_id: event.run_id.clone(),
                sequence: sequence as u64,
                event_type: event.event_type,
                causation_id: event.causation_id.clone(),
                operation_id: event.operation_id.clone(),
                timestamp: chrono::Utc::now().to_rfc3339(),
                payload: event.payload.clone(),
                processed: false,
            };
            let hash = event_hash(task_id, &event);
            claim_operation(
                tx,
                &event.operation_id,
                &format!("event:{}", event.event_type),
                task_id,
                &hash,
            )?;
            insert_event_conn(tx, &stored)?;
            let mut updated = record;
            updated.phase = transition.phase;
            updated.status = transition.status;
            updated.review_round = transition.review_round;
            if let Some(spec) = transition.spec {
                updated.spec = spec;
                updated.title = updated.spec.title.clone();
            }
            updated.spec_version = transition.spec_version;
            for (role, agent_status) in &transition.agent_updates {
                match role {
                    super::model::AgentRole::Coder => updated.coder.agent_status = *agent_status,
                    super::model::AgentRole::Reviewer => {
                        updated.reviewer.agent_status = *agent_status
                    }
                }
            }
            if let Some(cleanup) = transition.cleanup_status {
                updated.cleanup_status = cleanup;
            }
            updated.updated_at = chrono::Utc::now().to_rfc3339();
            update_task_conn(tx, &updated)?;
            if !transition.invalidated.is_empty() {
                let now = chrono::Utc::now().to_rfc3339();
                tx.execute(
                    "UPDATE sa_reports SET invalidated=1 WHERE task_id=?1 AND spec_version < ?2",
                    params![task_id, transition.spec_version],
                )?;
                tx.execute(
                    "UPDATE sa_approvals SET status=?2, resolved_at=?3 WHERE task_id=?1 AND status=?4 AND task_spec_version < ?5",
                    params![
                        task_id,
                        ApprovalStatus::Expired.as_str(),
                        now,
                        ApprovalStatus::Pending.as_str(),
                        transition.spec_version
                    ],
                )?;
            }
            Ok(EventAppend {
                event: stored,
                inserted: true,
            })
        })
    }

    pub fn complete_task(
        &self,
        task_id: &str,
        operation_id: &str,
        policy: &StatePolicy,
    ) -> Result<TaskRecord, StoreError> {
        self.apply_event(
            task_id,
            NewEvent {
                session_id: None,
                run_id: None,
                event_type: EventType::TaskCompleted,
                causation_id: None,
                operation_id: operation_id.into(),
                payload: json!({}),
            },
            policy,
        )?;
        self.get_task(task_id)?
            .ok_or_else(|| StoreError::TaskNotFound(task_id.into()))
    }

    pub fn set_sessions(
        &self,
        task_id: &str,
        coder_session_id: Option<&str>,
        reviewer_session_id: Option<&str>,
    ) -> Result<(), StoreError> {
        self.run_tx("super-agent set sessions", |tx| {
            let mut record = load_record_conn(tx, task_id)?
                .ok_or_else(|| StoreError::TaskNotFound(task_id.into()))?;
            if let Some(session) = coder_session_id {
                record.coder.session_id = Some(session.into());
            }
            if let Some(session) = reviewer_session_id {
                record.reviewer.session_id = Some(session.into());
            }
            record.updated_at = chrono::Utc::now().to_rfc3339();
            update_task_conn(tx, &record)
        })
    }

    pub fn replace_coder_session(&self, task_id: &str, session_id: &str) -> Result<(), StoreError> {
        self.run_tx("super-agent replace coder", |tx| {
            let mut record = load_record_conn(tx, task_id)?
                .ok_or_else(|| StoreError::TaskNotFound(task_id.into()))?;
            if let Some(previous) = record.coder.session_id.replace(session_id.into()) {
                record.coder.previous_session_ids.push(previous);
            }
            record.updated_at = chrono::Utc::now().to_rfc3339();
            update_task_conn(tx, &record)
        })
    }

    pub fn set_cleanup_status(
        &self,
        task_id: &str,
        status: CleanupStatus,
    ) -> Result<(), StoreError> {
        self.run_tx("super-agent cleanup status", |tx| {
            tx.execute(
                "UPDATE sa_tasks SET cleanup_status=?2, updated_at=?3 WHERE task_id=?1",
                params![task_id, status.as_str(), chrono::Utc::now().to_rfc3339()],
            )?;
            Ok(())
        })
    }

    pub fn events(&self, task_id: &str) -> Result<Vec<Event>, StoreError> {
        self.run_read(|tx| {
            let mut statement = tx.prepare(
                "SELECT event_id,task_id,pair_id,session_id,run_id,sequence,event_type,causation_id,operation_id,timestamp,payload_json,processed FROM sa_events WHERE task_id=?1 ORDER BY sequence",
            )?;
            let events = statement
                .query_map([task_id], event_from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut result = Vec::with_capacity(events.len());
            for event in events {
                result.push(event?);
            }
            Ok(result)
        })
    }

    pub fn add_report(
        &self,
        operation_id: &str,
        task_id: &str,
        kind: ReportKind,
        spec_version: u32,
        round: u32,
        body: Value,
    ) -> Result<Report, StoreError> {
        validate_operation_id(operation_id)?;
        if spec_version == 0 {
            return Err(StoreError::Invalid(
                "Report spec version must be positive".into(),
            ));
        }
        let encoded = serde_json::to_vec(&body)?;
        if encoded.len() > MAX_REPORT_BYTES {
            return Err(StoreError::Invalid("Report exceeds 2 MiB".into()));
        }
        let hash = hash_json(
            &json!({"task":task_id,"kind":kind,"spec":spec_version,"round":round,"body":body}),
        );
        self.run_tx("super-agent add report", |tx| {
            if let Some((stored_kind, stored_task, stored_hash)) =
                operation_row(tx, operation_id)?
            {
                if stored_kind == "report.add" && stored_task == task_id && stored_hash == hash {
                    return report_by_operation(tx, operation_id)?
                        .ok_or_else(|| StoreError::Invalid("report operation is missing".into()));
                }
                return Err(StoreError::OperationConflict {
                    operation_id: operation_id.into(),
                    kind: stored_kind,
                });
            }
            if load_record_conn(tx, task_id)?.is_none() {
                return Err(StoreError::TaskNotFound(task_id.into()));
            }
            let report = Report {
                report_id: uuid::Uuid::new_v4().to_string(),
                task_id: task_id.into(),
                kind,
                spec_version,
                round,
                body: body.clone(),
                created_at: chrono::Utc::now().to_rfc3339(),
                invalidated: false,
            };
            claim_operation(tx, operation_id, "report.add", task_id, &hash)?;
            tx.execute(
                "INSERT INTO sa_reports(report_id,operation_id,task_id,kind,spec_version,round,body_json,created_at,invalidated) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,0)",
                params![
                    report.report_id,
                    operation_id,
                    report.task_id,
                    report.kind.as_str(),
                    report.spec_version,
                    report.round,
                    serde_json::to_string(&report.body)?,
                    report.created_at
                ],
            )?;
            Ok(report)
        })
    }

    pub fn reports(&self, task_id: &str) -> Result<Vec<Report>, StoreError> {
        self.run_read(|tx| load_reports_conn(tx, task_id))
    }

    pub fn request_approval(
        &self,
        operation_id: &str,
        task_id: &str,
        draft: ApprovalDraft,
    ) -> Result<Approval, StoreError> {
        validate_operation_id(operation_id)?;
        let encoded = serde_json::to_vec(&draft.request)?;
        if encoded.len() > MAX_REPORT_BYTES {
            return Err(StoreError::Invalid("Approval request exceeds 2 MiB".into()));
        }
        let hash = hash_json(&json!({"task":task_id,"draft":{
            "source":draft.source_session_id,
            "parent":draft.parent_session_id,
            "type":draft.request_type,
            "request":draft.request,
            "spec":draft.task_spec_version,
            "policy":draft.permission_policy_version
        }}));
        self.run_tx("super-agent request approval", |tx| {
            if let Some((stored_kind, stored_task, stored_hash)) =
                operation_row(tx, operation_id)?
            {
                if stored_kind == "approval.request" && stored_task == task_id && stored_hash == hash
                {
                    return approval_by_operation(tx, operation_id)?.ok_or_else(|| {
                        StoreError::Invalid("approval operation is missing".into())
                    });
                }
                return Err(StoreError::OperationConflict {
                    operation_id: operation_id.into(),
                    kind: stored_kind,
                });
            }
            let record = load_record_conn(tx, task_id)?
                .ok_or_else(|| StoreError::TaskNotFound(task_id.into()))?;
            let approval = Approval {
                approval_id: uuid::Uuid::new_v4().to_string(),
                task_id: task_id.into(),
                pair_id: record.pair_id,
                source_session_id: draft.source_session_id.clone(),
                parent_session_id: draft.parent_session_id.clone(),
                request_type: draft.request_type,
                request: draft.request.clone(),
                task_spec_version: draft.task_spec_version,
                permission_policy_version: draft.permission_policy_version,
                status: ApprovalStatus::Pending,
                answer: None,
                created_at: chrono::Utc::now().to_rfc3339(),
                resolved_at: None,
            };
            claim_operation(tx, operation_id, "approval.request", task_id, &hash)?;
            tx.execute(
                "INSERT INTO sa_approvals(approval_id,operation_id,task_id,pair_id,source_session_id,parent_session_id,request_type,request_json,task_spec_version,permission_policy_version,status,answer_json,created_at,resolved_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,NULL,?12,NULL)",
                params![
                    approval.approval_id,
                    operation_id,
                    approval.task_id,
                    approval.pair_id,
                    approval.source_session_id,
                    approval.parent_session_id,
                    approval.request_type.as_str(),
                    serde_json::to_string(&approval.request)?,
                    approval.task_spec_version,
                    approval.permission_policy_version,
                    approval.status.as_str(),
                    approval.created_at
                ],
            )?;
            Ok(approval)
        })
    }

    pub fn resolve_approval(
        &self,
        operation_id: &str,
        approval_id: &str,
        decision: ApprovalStatus,
        note: Option<&str>,
    ) -> Result<Approval, StoreError> {
        validate_operation_id(operation_id)?;
        if !matches!(
            decision,
            ApprovalStatus::Approved | ApprovalStatus::Rejected
        ) {
            return Err(StoreError::Invalid(
                "Approval decisions are APPROVED or REJECTED".into(),
            ));
        }
        let hash = hash_json(&json!({"approval":approval_id,"decision":decision,"note":note}));
        self.run_tx("super-agent resolve approval", |tx| {
            let mut approval = approval_by_id(tx, approval_id)?
                .ok_or_else(|| StoreError::Invalid(format!("approval {approval_id} not found")))?;
            if let Some((stored_kind, stored_task, stored_hash)) =
                operation_row(tx, operation_id)?
            {
                if stored_kind == "approval.resolve"
                    && stored_task == approval.task_id
                    && stored_hash == hash
                {
                    return approval_by_operation(tx, operation_id)?.ok_or_else(|| {
                        StoreError::Invalid("approval resolution is missing".into())
                    });
                }
                return Err(StoreError::OperationConflict {
                    operation_id: operation_id.into(),
                    kind: stored_kind,
                });
            }
            if approval.status != ApprovalStatus::Pending {
                return Err(StoreError::AlreadyResolved(approval_id.into()));
            }
            claim_operation(
                tx,
                operation_id,
                "approval.resolve",
                &approval.task_id,
                &hash,
            )?;
            let answer = json!({"decision": decision.as_str(), "note": note});
            let resolved_at = chrono::Utc::now().to_rfc3339();
            tx.execute(
                "UPDATE sa_approvals SET status=?2, answer_json=?3, resolved_at=?4 WHERE approval_id=?1",
                params![
                    approval_id,
                    decision.as_str(),
                    serde_json::to_string(&answer)?,
                    resolved_at
                ],
            )?;
            approval.status = decision;
            approval.answer = Some(answer);
            approval.resolved_at = Some(resolved_at);
            Ok(approval)
        })
    }

    pub fn approvals(&self, task_id: &str) -> Result<Vec<Approval>, StoreError> {
        self.run_read(|tx| load_approvals_conn(tx, task_id))
    }

    pub fn try_lock(
        &self,
        operation_id: &str,
        lock_key: &str,
        task_id: &str,
    ) -> Result<LockOutcome, StoreError> {
        validate_operation_id(operation_id)?;
        validate_lock_key(lock_key)?;
        let hash = hash_json(&json!({"key":lock_key}));
        self.run_tx("super-agent acquire lock", |tx| {
            if let Some((kind, stored_task, stored_hash)) = operation_row(tx, operation_id)? {
                if kind == "lock" && stored_task == task_id && stored_hash == hash {
                    return Ok(LockOutcome::AlreadyOwned);
                }
                return Err(StoreError::OperationConflict {
                    operation_id: operation_id.into(),
                    kind,
                });
            }
            let holder: Option<(String, String)> = tx
                .query_row(
                    "SELECT task_id,owner_operation_id FROM sa_locks WHERE lock_key=?1",
                    [lock_key],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((holder_task, holder_operation)) = holder {
                if holder_task == task_id && holder_operation == operation_id {
                    return Ok(LockOutcome::AlreadyOwned);
                }
                return Err(StoreError::LockBusy(lock_key.into()));
            }
            claim_operation(tx, operation_id, "lock", task_id, &hash)?;
            tx.execute(
                "INSERT INTO sa_locks(lock_key,task_id,owner_operation_id,acquired_at) VALUES(?1,?2,?3,?4)",
                params![lock_key, task_id, operation_id, chrono::Utc::now().to_rfc3339()],
            )?;
            Ok(LockOutcome::Acquired)
        })
    }

    pub fn unlock(
        &self,
        lock_key: &str,
        task_id: &str,
        owner_operation_id: &str,
    ) -> Result<bool, StoreError> {
        self.run_tx("super-agent release lock", |tx| {
            let removed = tx.execute(
                "DELETE FROM sa_locks WHERE lock_key=?1 AND task_id=?2 AND owner_operation_id=?3",
                params![lock_key, task_id, owner_operation_id],
            )?;
            Ok(removed > 0)
        })
    }

    pub fn locks(&self, task_id: &str) -> Result<Vec<String>, StoreError> {
        self.run_read(|tx| {
            let mut statement =
                tx.prepare("SELECT lock_key FROM sa_locks WHERE task_id=?1 ORDER BY lock_key")?;
            let locks = statement
                .query_map([task_id], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(locks)
        })
    }

    pub fn save_repo(&self, repo: &RepoTarget) -> Result<(), StoreError> {
        self.run_tx("super-agent save repository", |tx| {
            upsert_repo_conn(tx, repo)
        })
    }

    /// Records all prepared targets in one transaction so a partially created
    /// environment never leaves a subset of targets behind.
    pub fn record_environment(
        &self,
        repos: &[RepoTarget],
        non_git_targets: &[NonGitTarget],
    ) -> Result<(), StoreError> {
        self.run_tx("super-agent record environment", |tx| {
            for repo in repos {
                upsert_repo_conn(tx, repo)?;
            }
            for target in non_git_targets {
                upsert_non_git_conn(tx, target)?;
            }
            Ok(())
        })
    }

    pub fn repos(&self, task_id: &str) -> Result<Vec<RepoTarget>, StoreError> {
        self.run_read(|tx| load_repos_conn(tx, task_id))
    }

    pub fn save_non_git_target(&self, target: &NonGitTarget) -> Result<(), StoreError> {
        self.run_tx("super-agent save non-Git target", |tx| {
            upsert_non_git_conn(tx, target)
        })
    }

    pub fn non_git_targets(&self, task_id: &str) -> Result<Vec<NonGitTarget>, StoreError> {
        self.run_read(|tx| load_non_git_conn(tx, task_id))
    }

    /// Writes a large artifact (log, diff, test output) atomically under the
    /// task's data directory. The report record keeps only a reference.
    pub fn write_artifact(
        &self,
        task_id: &str,
        name: &str,
        bytes: &[u8],
    ) -> Result<PathBuf, StoreError> {
        validate_task_id(task_id)?;
        validate_artifact_name(name)?;
        if bytes.len() > MAX_ARTIFACT_BYTES {
            return Err(StoreError::Invalid("Artifact exceeds 64 MiB".into()));
        }
        let directory = self
            .data_dir
            .join("super-agent")
            .join(task_id)
            .join("artifacts");
        std::fs::create_dir_all(&directory)?;
        let path = directory.join(name);
        let mut temporary = tempfile::NamedTempFile::new_in(&directory)?;
        std::io::Write::write_all(&mut temporary, bytes)?;
        std::io::Write::flush(&mut temporary)?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(&path)
            .map_err(|error| StoreError::Internal(error.error.into()))?;
        #[cfg(unix)]
        std::fs::File::open(&directory)?.sync_all()?;
        Ok(path)
    }
}

fn upsert_repo_conn(conn: &Connection, repo: &RepoTarget) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO sa_task_repos(task_id,repository_id,repo_root,git_common_dir,target_path,target_branch,task_branch,worktree_path,working_path,base_commit,submitted_commit,reviewed_commit,integrated_commit,integration_status,verification_status,source_state_json) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16) ON CONFLICT(task_id,repository_id) DO UPDATE SET repo_root=excluded.repo_root,git_common_dir=excluded.git_common_dir,target_path=excluded.target_path,target_branch=excluded.target_branch,task_branch=excluded.task_branch,worktree_path=excluded.worktree_path,working_path=excluded.working_path,base_commit=excluded.base_commit,submitted_commit=excluded.submitted_commit,reviewed_commit=excluded.reviewed_commit,integrated_commit=excluded.integrated_commit,integration_status=excluded.integration_status,verification_status=excluded.verification_status,source_state_json=excluded.source_state_json",
        params![
            repo.task_id,
            repo.repository_id,
            repo.repo_root.to_string_lossy(),
            repo.git_common_dir.to_string_lossy(),
            repo.target_path.to_string_lossy(),
            repo.target_branch,
            repo.task_branch,
            repo.worktree_path.to_string_lossy(),
            repo.working_path.to_string_lossy(),
            repo.base_commit,
            repo.submitted_commit,
            repo.reviewed_commit,
            repo.integrated_commit,
            repo.integration_status.as_str(),
            repo.verification_status.as_str(),
            serde_json::to_string(&repo.source_state)?
        ],
    )?;
    Ok(())
}

fn upsert_non_git_conn(conn: &Connection, target: &NonGitTarget) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO sa_non_git_targets(task_id,path,snapshot_dir,snapshot_manifest_json,diff_ref,status,lock_key) VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(task_id,path) DO UPDATE SET snapshot_dir=excluded.snapshot_dir,snapshot_manifest_json=excluded.snapshot_manifest_json,diff_ref=excluded.diff_ref,status=excluded.status,lock_key=excluded.lock_key",
        params![
            target.task_id,
            target.path.to_string_lossy(),
            target.snapshot_dir.to_string_lossy(),
            target.snapshot_manifest_json,
            target.diff_ref,
            target.status.as_str(),
            target.lock_key
        ],
    )?;
    Ok(())
}

fn validate_operation_id(value: &str) -> Result<(), StoreError> {
    if value.trim().is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        return Err(StoreError::Invalid(
            "Operation ID must contain 1-256 non-control characters".into(),
        ));
    }
    Ok(())
}

pub fn validate_task_id(value: &str) -> Result<(), StoreError> {
    let valid = !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
    if !valid {
        return Err(StoreError::Invalid("Invalid task ID".into()));
    }
    Ok(())
}

fn validate_lock_key(value: &str) -> Result<(), StoreError> {
    if value.trim().is_empty() || value.len() > 4_096 || value.chars().any(char::is_control) {
        return Err(StoreError::Invalid("Invalid lock key".into()));
    }
    Ok(())
}

fn validate_artifact_name(value: &str) -> Result<(), StoreError> {
    let valid = !value.is_empty()
        && value.len() <= 128
        && value != "."
        && value != ".."
        && !value.starts_with('.')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'));
    if !valid {
        return Err(StoreError::Invalid("Invalid artifact name".into()));
    }
    Ok(())
}

fn hash_json(value: &Value) -> String {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn event_hash(task_id: &str, event: &NewEvent) -> String {
    hash_json(&json!({
        "task":task_id,
        "type":event.event_type,
        "session":event.session_id,
        "run":event.run_id,
        "causation":event.causation_id,
        "payload":event.payload
    }))
}

fn operation_row(
    conn: &Connection,
    operation_id: &str,
) -> Result<Option<(String, String, String)>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT kind,task_id,payload_hash FROM sa_operations WHERE operation_id=?1",
            [operation_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?)
}

fn claim_operation(
    conn: &Connection,
    operation_id: &str,
    kind: &str,
    task_id: &str,
    payload_hash: &str,
) -> Result<(), StoreError> {
    match operation_row(conn, operation_id)? {
        Some((stored_kind, stored_task, stored_hash))
            if stored_kind == kind && stored_task == task_id && stored_hash == payload_hash =>
        {
            Ok(())
        }
        Some((stored_kind, _, _)) => Err(StoreError::OperationConflict {
            operation_id: operation_id.into(),
            kind: stored_kind,
        }),
        None => {
            conn.execute(
                "INSERT INTO sa_operations(operation_id,kind,task_id,payload_hash,created_at) VALUES(?1,?2,?3,?4,?5)",
                params![operation_id, kind, task_id, payload_hash, chrono::Utc::now().to_rfc3339()],
            )?;
            Ok(())
        }
    }
}

fn next_task_number(conn: &Connection) -> Result<u32, StoreError> {
    let mut maximum = 0u32;
    for column in ["task_id", "pair_id"] {
        let mut statement = conn.prepare(&format!("SELECT {column} FROM sa_tasks"))?;
        let values = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for value in values {
            if let Some((_, number)) = value.rsplit_once('-') {
                if let Ok(number) = number.parse::<u32>() {
                    maximum = maximum.max(number);
                }
            }
        }
    }
    maximum
        .checked_add(1)
        .ok_or_else(|| StoreError::Invalid("Task ID space exhausted".into()))
}

struct RawTask {
    task_id: String,
    router_session_id: String,
    pair_id: String,
    title: String,
    spec_json: String,
    spec_version: u32,
    phase: String,
    status: String,
    review_round: u32,
    parent_task_id: Option<String>,
    depends_on_json: String,
    coder_session_id: Option<String>,
    coder_previous_json: String,
    coder_agent_status: String,
    reviewer_session_id: Option<String>,
    reviewer_agent_status: String,
    cleanup_status: String,
    permission_policy_version: u32,
    created_at: String,
    updated_at: String,
}

fn load_record_conn(conn: &Connection, task_id: &str) -> Result<Option<TaskRecord>, StoreError> {
    let raw = conn
        .query_row(
            "SELECT task_id,router_session_id,pair_id,title,spec_json,spec_version,phase,status,review_round,parent_task_id,depends_on_json,coder_session_id,coder_previous_json,coder_agent_status,reviewer_session_id,reviewer_agent_status,cleanup_status,permission_policy_version,created_at,updated_at FROM sa_tasks WHERE task_id=?1",
            [task_id],
            |row| {
                Ok(RawTask {
                    task_id: row.get(0)?,
                    router_session_id: row.get(1)?,
                    pair_id: row.get(2)?,
                    title: row.get(3)?,
                    spec_json: row.get(4)?,
                    spec_version: row.get::<_, i64>(5)? as u32,
                    phase: row.get(6)?,
                    status: row.get(7)?,
                    review_round: row.get::<_, i64>(8)? as u32,
                    parent_task_id: row.get(9)?,
                    depends_on_json: row.get(10)?,
                    coder_session_id: row.get(11)?,
                    coder_previous_json: row.get(12)?,
                    coder_agent_status: row.get(13)?,
                    reviewer_session_id: row.get(14)?,
                    reviewer_agent_status: row.get(15)?,
                    cleanup_status: row.get(16)?,
                    permission_policy_version: row.get::<_, i64>(17)? as u32,
                    created_at: row.get(18)?,
                    updated_at: row.get(19)?,
                })
            },
        )
        .optional()?;
    let Some(raw) = raw else {
        return Ok(None);
    };
    let spec: TaskSpec = serde_json::from_str(&raw.spec_json)
        .with_context(|| format!("Invalid task spec for {}", raw.task_id))?;
    let mut record = TaskRecord {
        task_id: raw.task_id,
        pair_id: raw.pair_id,
        router_session_id: raw.router_session_id,
        title: raw.title,
        spec,
        spec_version: raw.spec_version,
        phase: Phase::parse(&raw.phase)?,
        status: Status::parse(&raw.status)?,
        review_round: raw.review_round,
        parent_task_id: raw.parent_task_id,
        depends_on: serde_json::from_str(&raw.depends_on_json)
            .context("Invalid task dependencies")?,
        coder: SessionRef {
            session_id: raw.coder_session_id,
            previous_session_ids: serde_json::from_str(&raw.coder_previous_json)
                .context("Invalid coder session history")?,
            agent_status: AgentStatus::parse(&raw.coder_agent_status)?,
        },
        reviewer: SessionRef {
            session_id: raw.reviewer_session_id,
            previous_session_ids: Vec::new(),
            agent_status: AgentStatus::parse(&raw.reviewer_agent_status)?,
        },
        cleanup_status: CleanupStatus::parse(&raw.cleanup_status)?,
        permission_policy_version: raw.permission_policy_version,
        created_at: raw.created_at,
        updated_at: raw.updated_at,
        repositories: load_repos_conn(conn, task_id)?,
        non_git_targets: load_non_git_conn(conn, task_id)?,
    };
    // The reviewer history is stored with the coder's key layout only in older
    // databases; this row is authoritative, so keep the field deterministic.
    record.reviewer.previous_session_ids = Vec::new();
    Ok(Some(record))
}

fn insert_task_conn(conn: &Connection, record: &TaskRecord) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO sa_tasks(task_id,router_session_id,pair_id,title,spec_json,spec_version,phase,status,review_round,parent_task_id,depends_on_json,coder_session_id,coder_previous_json,coder_agent_status,reviewer_session_id,reviewer_agent_status,cleanup_status,permission_policy_version,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20)",
        params![
            record.task_id,
            record.router_session_id,
            record.pair_id,
            record.title,
            serde_json::to_string(&record.spec)?,
            record.spec_version,
            record.phase.as_str(),
            record.status.as_str(),
            record.review_round,
            record.parent_task_id,
            serde_json::to_string(&record.depends_on)?,
            record.coder.session_id,
            serde_json::to_string(&record.coder.previous_session_ids)?,
            record.coder.agent_status.as_str(),
            record.reviewer.session_id,
            record.reviewer.agent_status.as_str(),
            record.cleanup_status.as_str(),
            record.permission_policy_version,
            record.created_at,
            record.updated_at
        ],
    )?;
    Ok(())
}

fn update_task_conn(conn: &Connection, record: &TaskRecord) -> Result<(), StoreError> {
    conn.execute(
        "UPDATE sa_tasks SET title=?2,spec_json=?3,spec_version=?4,phase=?5,status=?6,review_round=?7,parent_task_id=?8,depends_on_json=?9,coder_session_id=?10,coder_previous_json=?11,coder_agent_status=?12,reviewer_session_id=?13,reviewer_agent_status=?14,cleanup_status=?15,permission_policy_version=?16,updated_at=?17 WHERE task_id=?1",
        params![
            record.task_id,
            record.title,
            serde_json::to_string(&record.spec)?,
            record.spec_version,
            record.phase.as_str(),
            record.status.as_str(),
            record.review_round,
            record.parent_task_id,
            serde_json::to_string(&record.depends_on)?,
            record.coder.session_id,
            serde_json::to_string(&record.coder.previous_session_ids)?,
            record.coder.agent_status.as_str(),
            record.reviewer.session_id,
            record.reviewer.agent_status.as_str(),
            record.cleanup_status.as_str(),
            record.permission_policy_version,
            record.updated_at
        ],
    )?;
    Ok(())
}

fn insert_event_conn(conn: &Connection, event: &Event) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO sa_events(event_id,task_id,pair_id,session_id,run_id,sequence,event_type,causation_id,operation_id,timestamp,payload_json,processed) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
        params![
            event.event_id,
            event.task_id,
            event.pair_id,
            event.session_id,
            event.run_id,
            event.sequence as i64,
            event.event_type.as_str(),
            event.causation_id,
            event.operation_id,
            event.timestamp,
            serde_json::to_string(&event.payload)?,
            event.processed as i64
        ],
    )?;
    Ok(())
}

type StoredEvent = Result<Event, StoreError>;

fn event_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredEvent> {
    let payload: String = row.get(10)?;
    let event_type: String = row.get(6)?;
    Ok((|| -> Result<Event, StoreError> {
        Ok(Event {
            event_id: row.get(0)?,
            task_id: row.get(1)?,
            pair_id: row.get(2)?,
            session_id: row.get(3)?,
            run_id: row.get(4)?,
            sequence: row.get::<_, i64>(5)? as u64,
            event_type: EventType::parse(&event_type)
                .map_err(|error| StoreError::Invalid(error.to_string()))?,
            causation_id: row.get(7)?,
            operation_id: row.get(8)?,
            timestamp: row.get(9)?,
            payload: serde_json::from_str(&payload)?,
            processed: row.get::<_, i64>(11)? != 0,
        })
    })())
}

fn event_by_operation(conn: &Connection, operation_id: &str) -> Result<Option<Event>, StoreError> {
    let row = conn
        .query_row(
            "SELECT event_id,task_id,pair_id,session_id,run_id,sequence,event_type,causation_id,operation_id,timestamp,payload_json,processed FROM sa_events WHERE operation_id=?1",
            [operation_id],
            event_from_row,
        )
        .optional()?;
    match row {
        Some(event) => Ok(Some(event?)),
        None => Ok(None),
    }
}

fn load_repos_conn(conn: &Connection, task_id: &str) -> Result<Vec<RepoTarget>, StoreError> {
    let mut statement = conn.prepare(
        "SELECT repository_id,repo_root,git_common_dir,target_path,target_branch,task_branch,worktree_path,working_path,base_commit,submitted_commit,reviewed_commit,integrated_commit,integration_status,verification_status,source_state_json FROM sa_task_repos WHERE task_id=?1 ORDER BY repository_id",
    )?;
    let raw = statement
        .query_map([task_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, String>(8)?,
                row.get::<_, Option<String>>(9)?,
                row.get::<_, Option<String>>(10)?,
                row.get::<_, Option<String>>(11)?,
                row.get::<_, String>(12)?,
                row.get::<_, String>(13)?,
                row.get::<_, String>(14)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raw.into_iter()
        .map(
            |(
                repository_id,
                repo_root,
                git_common_dir,
                target_path,
                target_branch,
                task_branch,
                worktree_path,
                working_path,
                base_commit,
                submitted_commit,
                reviewed_commit,
                integrated_commit,
                integration_status,
                verification_status,
                source_state_json,
            )| {
                Ok(RepoTarget {
                    task_id: task_id.into(),
                    repository_id,
                    repo_root: repo_root.into(),
                    git_common_dir: git_common_dir.into(),
                    target_path: target_path.into(),
                    target_branch,
                    task_branch,
                    worktree_path: worktree_path.into(),
                    working_path: working_path.into(),
                    base_commit,
                    submitted_commit,
                    reviewed_commit,
                    integrated_commit,
                    integration_status: IntegrationStatus::parse(&integration_status)
                        .map_err(|error| StoreError::Invalid(error.to_string()))?,
                    verification_status: VerificationStatus::parse(&verification_status)
                        .map_err(|error| StoreError::Invalid(error.to_string()))?,
                    source_state: serde_json::from_str(&source_state_json)?,
                })
            },
        )
        .collect()
}

fn load_non_git_conn(conn: &Connection, task_id: &str) -> Result<Vec<NonGitTarget>, StoreError> {
    let mut statement = conn.prepare(
        "SELECT path,snapshot_dir,snapshot_manifest_json,diff_ref,status,lock_key FROM sa_non_git_targets WHERE task_id=?1 ORDER BY path",
    )?;
    let raw = statement
        .query_map([task_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raw.into_iter()
        .map(
            |(path, snapshot_dir, snapshot_manifest_json, diff_ref, status, lock_key)| {
                Ok(NonGitTarget {
                    task_id: task_id.into(),
                    path: path.into(),
                    snapshot_dir: snapshot_dir.into(),
                    snapshot_manifest_json,
                    diff_ref,
                    status: NonGitStatus::parse(&status)
                        .map_err(|error| StoreError::Invalid(error.to_string()))?,
                    lock_key,
                })
            },
        )
        .collect()
}

fn load_reports_conn(conn: &Connection, task_id: &str) -> Result<Vec<Report>, StoreError> {
    let mut statement = conn.prepare(
        "SELECT report_id,kind,spec_version,round,body_json,created_at,invalidated FROM sa_reports WHERE task_id=?1 ORDER BY created_at, report_id",
    )?;
    let raw = statement
        .query_map([task_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)? as u32,
                row.get::<_, i64>(3)? as u32,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, i64>(6)? != 0,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raw.into_iter()
        .map(
            |(report_id, kind, spec_version, round, body_json, created_at, invalidated)| {
                Ok(Report {
                    report_id,
                    task_id: task_id.into(),
                    kind: ReportKind::parse(&kind)
                        .map_err(|error| StoreError::Invalid(error.to_string()))?,
                    spec_version,
                    round,
                    body: serde_json::from_str(&body_json)?,
                    created_at,
                    invalidated,
                })
            },
        )
        .collect()
}

fn report_by_operation(
    conn: &Connection,
    operation_id: &str,
) -> Result<Option<Report>, StoreError> {
    let row: Option<(String, String, String, u32, u32, String, String, bool)> = conn
        .query_row(
            "SELECT report_id,task_id,kind,spec_version,round,body_json,created_at,invalidated FROM sa_reports WHERE operation_id=?1",
            [operation_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get::<_, i64>(3)? as u32,
                    row.get::<_, i64>(4)? as u32,
                    row.get(5)?,
                    row.get(6)?,
                    row.get::<_, i64>(7)? != 0,
                ))
            },
        )
        .optional()?;
    row.map(
        |(report_id, task_id, kind, spec_version, round, body_json, created_at, invalidated)| {
            Ok(Report {
                report_id,
                task_id,
                kind: ReportKind::parse(&kind)
                    .map_err(|error| StoreError::Invalid(error.to_string()))?,
                spec_version,
                round,
                body: serde_json::from_str(&body_json)?,
                created_at,
                invalidated,
            })
        },
    )
    .transpose()
}

fn load_approvals_conn(conn: &Connection, task_id: &str) -> Result<Vec<Approval>, StoreError> {
    let mut statement = conn.prepare(
        "SELECT approval_id,task_id,pair_id,source_session_id,parent_session_id,request_type,request_json,task_spec_version,permission_policy_version,status,answer_json,created_at,resolved_at FROM sa_approvals WHERE task_id=?1 ORDER BY created_at, approval_id",
    )?;
    let rows = statement
        .query_map([task_id], approval_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut result = Vec::with_capacity(rows.len());
    for approval in rows {
        result.push(approval?);
    }
    Ok(result)
}

fn approval_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Result<Approval, StoreError>> {
    let request_type: String = row.get(5)?;
    let status: String = row.get(9)?;
    let request_json: String = row.get(6)?;
    let answer_json: Option<String> = row.get(10)?;
    Ok((|| -> Result<Approval, StoreError> {
        Ok(Approval {
            approval_id: row.get(0)?,
            task_id: row.get(1)?,
            pair_id: row.get(2)?,
            source_session_id: row.get(3)?,
            parent_session_id: row.get(4)?,
            request_type: ApprovalType::parse(&request_type)
                .map_err(|error| StoreError::Invalid(error.to_string()))?,
            request: serde_json::from_str(&request_json)?,
            task_spec_version: row.get::<_, i64>(7)? as u32,
            permission_policy_version: row.get::<_, i64>(8)? as u32,
            status: ApprovalStatus::parse(&status)
                .map_err(|error| StoreError::Invalid(error.to_string()))?,
            answer: answer_json
                .map(|value| serde_json::from_str(&value))
                .transpose()?,
            created_at: row.get(11)?,
            resolved_at: row.get(12)?,
        })
    })())
}

fn approval_by_id(conn: &Connection, approval_id: &str) -> Result<Option<Approval>, StoreError> {
    let row = conn
        .query_row(
            "SELECT approval_id,task_id,pair_id,source_session_id,parent_session_id,request_type,request_json,task_spec_version,permission_policy_version,status,answer_json,created_at,resolved_at FROM sa_approvals WHERE approval_id=?1",
            [approval_id],
            approval_from_row,
        )
        .optional()?;
    match row {
        Some(approval) => Ok(Some(approval?)),
        None => Ok(None),
    }
}

fn approval_by_operation(
    conn: &Connection,
    operation_id: &str,
) -> Result<Option<Approval>, StoreError> {
    let row = conn
        .query_row(
            "SELECT approval_id,task_id,pair_id,source_session_id,parent_session_id,request_type,request_json,task_spec_version,permission_policy_version,status,answer_json,created_at,resolved_at FROM sa_approvals WHERE operation_id=?1",
            [operation_id],
            approval_from_row,
        )
        .optional()?;
    match row {
        Some(approval) => Ok(Some(approval?)),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests;
