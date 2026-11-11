//! Nonblocking goal consent. A model proposal is never human authorization.
use crate::{
    db::DbManager,
    permissions::{PermissionContext, PermissionManager, PermissionMode, PermissionResult},
};
use anyhow::{ensure, Context, Result};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    path::Path,
    sync::{
        atomic::{AtomicI64, Ordering},
        Arc,
    },
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Proposal {
    pub id: String,
    pub condition: String,
    revision: u64,
    frontend: String,
    pub approved: bool,
    #[serde(default)]
    source_job: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Presence {
    id: String,
    touched_ms: i64,
}
#[derive(Default, Serialize, Deserialize)]
struct State {
    frontend: Option<Presence>,
    pending: Option<Proposal>,
    declined: Vec<String>,
}
fn now() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
fn read(conn: &Connection, session: &str) -> Result<State> {
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM peripheral_settings WHERE key=?1",
            [format!("session.goal-proposals.{session}")],
            |r| r.get(0),
        )
        .optional()?;
    Ok(value
        .map(|s| serde_json::from_str(&s))
        .transpose()?
        .unwrap_or_default())
}
fn update<T>(
    db: &DbManager,
    session: &str,
    f: impl FnOnce(&Connection, &mut State) -> Result<T>,
) -> Result<T> {
    db.with_conn(|conn| update_conn(conn, session, f))?
}
fn update_conn<T>(conn: &Connection, session: &str, f: impl FnOnce(&Connection, &mut State) -> Result<T>) -> Result<T> {
    let tx = crate::db::write_transaction(conn)?;
    let mut state = read(&tx, session)?;
    let result = f(&tx, &mut state)?;
    tx.execute("INSERT INTO peripheral_settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [format!("session.goal-proposals.{session}"), serde_json::to_string(&state)?])?;
    tx.commit()?;
    Ok(result)
}
fn live(frontend: &Presence) -> bool {
    now().saturating_sub(frontend.touched_ms) < 5_000
}
fn prune(conn: &Connection, session: &str, state: &mut State) -> Result<()> {
    let revision = super::session_goals::revision_conn(conn, session)?;
    if state.pending.as_ref().is_some_and(|p| {
        p.revision != revision
            || (!p.approved
                && !state
                    .frontend
                    .as_ref()
                    .is_some_and(|f| f.id == p.frontend && live(f)))
    }) {
        state.pending = None;
    }
    Ok(())
}

/// Only the actual local terminal owner constructs this lease. Worker and SDK
/// processes cannot infer interactivity from inherited environment variables.
pub struct Frontend {
    db: Arc<DbManager>,
    session: String,
    id: String,
    heartbeat: AtomicI64,
}
impl Frontend {
    pub fn acquire(db: Arc<DbManager>, session: &str) -> Result<Self> {
        let id = uuid::Uuid::new_v4().to_string();
        update(&db, session, |_, state| {
            ensure!(
                !state.frontend.as_ref().is_some_and(live),
                "Another local frontend owns goal proposals for this session"
            );
            state.frontend = Some(Presence {
                id: id.clone(),
                touched_ms: now(),
            });
            Ok(())
        })?;
        Ok(Self {
            db,
            session: session.into(),
            id,
            heartbeat: AtomicI64::new(now()),
        })
    }
    pub fn poll(&self) -> Result<Option<Proposal>> {
        let current = now();
        if current.saturating_sub(self.heartbeat.load(Ordering::Relaxed)) < 1_000 {
            return self.db.with_conn(|conn| -> Result<Option<Proposal>> {
                let state = read(conn, &self.session)?;
                ensure!(
                    state
                        .frontend
                        .as_ref()
                        .is_some_and(|f| f.id == self.id && live(f)),
                    "Goal proposal frontend ownership changed"
                );
                let Some(proposal) = state
                    .pending
                    .filter(|p| !p.approved && p.frontend == self.id)
                else {
                    return Ok(None);
                };
                Ok(
                    (proposal.revision
                        == super::session_goals::revision_conn(conn, &self.session)?)
                    .then_some(proposal),
                )
            })?;
        }
        let proposal = self.db.with_conn(|conn| -> Result<_> {
            // A view heartbeat must not block event rendering for the normal write timeout.
            let timeout: u64 = conn.query_row("PRAGMA busy_timeout", [], |row| row.get(0))?;
            conn.busy_timeout(std::time::Duration::from_millis(25))?;
            let result = update_conn(conn, &self.session, |conn, state| {
                // Expired consent must not be revived by a recovered heartbeat.
                prune(conn, &self.session, state)?;
                let frontend = state.frontend.as_mut().filter(|f| f.id == self.id)
                    .context("Goal proposal frontend ownership changed")?;
                frontend.touched_ms = now();
                Ok(state.pending.as_ref().filter(|p| !p.approved).cloned())
            });
            let restored = conn.busy_timeout(std::time::Duration::from_millis(timeout));
            let value = result?;
            restored?;
            Ok(value)
        })??;
        // Failed heartbeats remain due, including after the previous lease expires.
        self.heartbeat.store(now(), Ordering::Relaxed);
        Ok(proposal)
    }
    pub fn decide(
        &self,
        id: &str,
        approved: bool,
        root: &Path,
        workspace: &Path,
        permissions: &PermissionManager,
    ) -> Result<bool> {
        let policy = if approved {
            let condition = self.db.with_conn(|conn| -> Result<_> {
                Ok(read(conn, &self.session)?
                    .pending
                    .filter(|p| p.id == id && !p.approved)
                    .map(|p| p.condition))
            })??;
            let Some(condition) = condition else {
                return Ok(false);
            };
            Some(review_policy(
                permissions,
                &self.session,
                workspace,
                &condition,
            )?)
        } else {
            None
        };
        update(&self.db, &self.session, |conn, state| {
            prune(conn, &self.session, state)?;
            ensure!(
                state
                    .frontend
                    .as_ref()
                    .is_some_and(|f| f.id == self.id && live(f)),
                "Goal proposal frontend is no longer active"
            );
            let Some(proposal) = state
                .pending
                .as_mut()
                .filter(|p| p.id == id && p.frontend == self.id && !p.approved)
            else {
                return Ok(false);
            };
            if approved {
                ensure!(
                    source_job_allows_activation(root, &self.session, proposal),
                    "The proposing task was cancelled or failed"
                );
                ensure!(
                    policy == Some(permissions.policy_snapshot()),
                    "ProposeGoal tool policy changed"
                );
                guard(
                    conn,
                    &self.session,
                    root,
                    workspace,
                    permissions,
                    &proposal.condition,
                )?;
                proposal.approved = true;
            } else {
                state.declined.push(proposal.condition.to_lowercase());
                state.pending = None;
            }
            Ok(true)
        })
    }
}
impl Drop for Frontend {
    fn drop(&mut self) {
        let _ = update(&self.db, &self.session, |_, state| {
            if state.frontend.as_ref().is_some_and(|f| f.id == self.id) {
                state.frontend = None;
                if state
                    .pending
                    .as_ref()
                    .is_some_and(|p| !p.approved && p.frontend == self.id)
                {
                    state.pending = None;
                }
            }
            Ok(())
        });
    }
}

fn settings(path: &Path) -> Result<Value> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(serde_json::from_str(text.trim_start_matches('\u{feff}'))?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(json!({})),
        Err(e) => Err(e.into()),
    }
}
fn guard(
    conn: &Connection,
    session: &str,
    root: &Path,
    workspace: &Path,
    permissions: &PermissionManager,
    _condition: &str,
) -> Result<bool> {
    if let Some(directory) = super::background_sessions::worker_directory(root, session)? {
        ensure!(
            !directory.join("cancel").exists(),
            "The proposing task was cancelled"
        );
    }
    crate::native_engine::workspace::conversation_project(conn, session)?;
    let mode: Option<String> = conn
        .query_row(
            "SELECT value FROM peripheral_settings WHERE key=?1",
            [format!("cli.permission-mode.{session}")],
            |r| r.get(0),
        )
        .optional()?;
    ensure!(
        permissions.get_mode() != PermissionMode::PlanMode && mode.as_deref() != Some("plan"),
        "ProposeGoal is unavailable in plan mode"
    );
    let user = settings(&root.join("settings.json"))?;
    let preference = user["modelProposedGoals"].as_str().unwrap_or("auto");
    ensure!(
        matches!(preference, "auto" | "alwaysAsk"),
        "Model-proposed goals are disabled or their user setting is invalid; use /goal explicitly"
    );
    // Consent settings above intentionally ignore project/local files.
    for value in [
        user.clone(),
        settings(&workspace.join(".claude/settings.json"))?,
        settings(&workspace.join(".claude/settings.local.json"))?,
    ] {
        ensure!(
            value["disableAllHooks"] != true && value["allowManagedHooksOnly"] != true,
            "Goal evaluator hooks are disabled or restricted"
        );
    }
    Ok(preference == "alwaysAsk")
}

// A saved-approval checker may query the same database. Evaluate outside its
// transaction and recheck the policy revision inside the consent transaction.
fn review_policy(
    permissions: &PermissionManager,
    session: &str,
    workspace: &Path,
    condition: &str,
) -> Result<crate::permissions::manager::PolicySnapshot> {
    let policy = permissions.policy_snapshot();
    let context = PermissionContext {
        mcp_read_only: false,
        tool_name: "ProposeGoal".into(),
        tool_input: json!({"condition":condition}),
        conversation_id: session.into(),
        user_id: None,
        workspace_path: Some(workspace.to_string_lossy().into_owned()),
    };
    match permissions.check_permission(&context) {
        PermissionResult::Granted => {}
        PermissionResult::Denied(reason) | PermissionResult::RequiresConfirmation(reason) => {
            anyhow::bail!("ProposeGoal tool policy: {reason}")
        }
    }
    ensure!(
        permissions.policy_snapshot() == policy,
        "ProposeGoal tool policy changed"
    );
    Ok(policy)
}
pub fn available(
    db: &DbManager,
    session: &str,
    root: &Path,
    workspace: &Path,
    permissions: &PermissionManager,
) -> bool {
    let Ok(policy) = review_policy(permissions, session, workspace, "") else {
        return false;
    };
    db.with_conn(|conn| -> Result<bool> {
        let state = read(conn, session)?;
        Ok(permissions.policy_snapshot() == policy
            && state.frontend.as_ref().is_some_and(live)
            && guard(conn, session, root, workspace, permissions, "").is_ok())
    })
    .ok()
    .and_then(Result::ok)
    .unwrap_or(false)
}
fn canonical_condition(condition: &str) -> Result<String> {
    let visible: String = condition.chars().filter(|c| !matches!(*c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}') && (!c.is_control() || c.is_whitespace())).collect();
    let visible = visible.split_whitespace().collect::<Vec<_>>().join(" ");
    ensure!(
        !visible.is_empty() && visible.encode_utf16().count() <= 500,
        "Proposed goal must contain 1 to 500 visible UTF-16 code units"
    );
    ensure!(
        !matches!(
            visible.to_ascii_lowercase().as_str(),
            "clear" | "stop" | "off" | "reset" | "none" | "cancel"
        ),
        "ProposeGoal cannot clear goals; the user may use /goal clear"
    );
    Ok(visible)
}
pub fn propose(
    db: &DbManager,
    session: &str,
    root: &Path,
    workspace: &Path,
    permissions: &PermissionManager,
    input: &Value,
) -> Result<Value> {
    let condition = canonical_condition(
        input["condition"]
            .as_str()
            .context("Goal condition is required")?,
    )?;
    let ask = input
        .get("ask_user")
        .map(|v| v.as_bool().context("ask_user must be a boolean"))
        .transpose()?
        .unwrap_or(true);
    let policy = review_policy(permissions, session, workspace, &condition)?;
    update(db, session, |conn, state| {
        ensure!(
            permissions.policy_snapshot() == policy,
            "ProposeGoal tool policy changed"
        );
        prune(conn, session, state)?;
        let always_ask = guard(conn, session, root, workspace, permissions, &condition)?;
        let frontend = state
            .frontend
            .as_ref()
            .filter(|f| live(f))
            .context("ProposeGoal requires an attached local interactive terminal")?;
        ensure!(state.pending.is_none(), "A goal proposal is already awaiting a decision or activation; keep working without waiting");
        ensure!(
            state.declined.len() < 256 && !state.declined.contains(&condition.to_lowercase()),
            "Do not repeat a declined goal proposal; continue the existing task"
        );
        if !ask && !always_ask {
            let goal: Option<String> = conn
                .query_row(
                    "SELECT value FROM peripheral_settings WHERE key=?1",
                    [format!("session.goal.{session}")],
                    |r| r.get(0),
                )
                .optional()?;
            let current: Value = goal
                .map(|s| serde_json::from_str(&s))
                .transpose()?
                .unwrap_or(Value::Null);
            if current["active"]["state"] == "active" && current["active"]["condition"] == condition
            {
                return Ok(
                    json!({"condition":condition,"askUser":false,"content":"The same explicitly authorized goal is already active. Continue working within its scope; no new authority or goal was created."}),
                );
            }
        }
        let proposal = Proposal {
            id: uuid::Uuid::new_v4().to_string(),
            condition: condition.clone(),
            revision: super::session_goals::revision_conn(conn, session)?,
            frontend: frontend.id.clone(),
            approved: false,
            source_job: super::background_sessions::worker_directory(root, session)?.and_then(
                |directory| {
                    directory
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                },
            ),
        };
        state.pending = Some(proposal);
        Ok(
            json!({"condition":condition,"askUser":true,"content":"Shown the goal proposal for approval. Continue working; do not wait for the decision. Until a kickoff message confirms activation, the existing goal remains unchanged. Do not ask about a declined proposal or re-propose it."}),
        )
    })
}

// Persist the originating job so a killed/detached worker cannot leave consent
// for a later unrelated turn to consume. Successful detached work remains valid.
fn source_job_allows_activation(root: &Path, session: &str, proposal: &Proposal) -> bool {
    let Some(id) = &proposal.source_job else {
        return true;
    };
    super::background_sessions::get(root, id).is_ok_and(|job| {
        job.session_id == session
            && super::session_worker::job_execution_session(root, &job)
                .is_ok_and(|target| target == session)
            && matches!(job.status.as_str(), "starting" | "running" | "completed")
            && !root
                .join("background-tasks")
                .join(id)
                .join("cancel")
                .exists()
    })
}

/// Consume a proven frontend approval exactly once at an execution boundary.
/// This is the only path that converts a proposal into human authority.
pub fn activate(
    db: &DbManager,
    session: &str,
    root: &Path,
    workspace: &Path,
    permissions: &PermissionManager,
) -> Result<Option<String>> {
    activate_at_boundary(db, session, root, workspace, permissions, None)
}

/// A frontend may only complete consent originating from the job it observed.
/// Missing source metadata is retained for compatibility with older proposals.
pub fn activate_from_job(
    db: &DbManager,
    session: &str,
    root: &Path,
    workspace: &Path,
    permissions: &PermissionManager,
    job: &str,
) -> Result<Option<String>> {
    activate_at_boundary(db, session, root, workspace, permissions, Some(job))
}

fn activate_at_boundary(
    db: &DbManager,
    session: &str,
    root: &Path,
    workspace: &Path,
    permissions: &PermissionManager,
    expected_job: Option<&str>,
) -> Result<Option<String>> {
    let pending = db.with_conn(|conn| -> Result<_> {
        Ok(read(conn, session)?.pending.filter(|p| p.approved))
    })??;
    let Some(pending) = pending else {
        return Ok(None);
    };
    let policy = review_policy(permissions, session, workspace, &pending.condition);
    update(db, session, |conn, state| {
        prune(conn, session, state)?;
        let Some(proposal) = state
            .pending
            .as_ref()
            .filter(|p| p.approved && p.id == pending.id)
        else {
            return Ok(None);
        };
        if expected_job.is_some_and(|expected| {
            proposal
                .source_job
                .as_deref()
                .is_some_and(|source| source != expected)
        }) {
            return Ok(None);
        }
        if !source_job_allows_activation(root, session, proposal)
            || !policy
                .as_ref()
                .is_ok_and(|snapshot| *snapshot == permissions.policy_snapshot())
            || guard(
                conn,
                session,
                root,
                workspace,
                permissions,
                &proposal.condition,
            )
            .is_err()
        {
            state.pending = None;
            return Ok(None);
        }
        let condition = proposal.condition.clone();
        super::session_goals::set_conn(conn, session, &condition)?;
        super::permission_authority::record_conn(conn, session, super::permission_authority::Source::User, &json!(format!("I approved this proposed session goal: {condition}. Work within my existing constraints; this approval does not grant additional tool permissions.")))?;
        state.pending = None;
        Ok(Some(format!("The user approved this session goal: {condition}\nThe goal is now active. Continue the existing work toward it without repeating completed operations and stay within all existing permissions.")))
    })
}

/// A Stop hook may forbid every form of automatic continuation, including a
/// proposal approved during the turn but not yet activated.
pub fn discard(db: &DbManager, session: &str) -> Result<()> {
    update(db, session, |_, state| {
        state.pending = None;
        Ok(())
    })
}
pub fn definition() -> crate::tools::ToolDefinition {
    crate::tools::ToolDefinition {
        name: "ProposeGoal".into(),
        description: "Propose a measurable completion condition (up to 500 UTF-16 code units) for a multi-turn task already requested by the user. Nonblocking: continue working while the user decides. Do not widen scope or repeat declined/reworded goals. ask_user defaults true. Setting false only reuses an exactly matching currently active goal; otherwise user approval remains required. No new goal or authority exists until a kickoff message confirms activation.".into(),
        input_schema: json!({"type":"object","properties":{"condition":{"type":"string","minLength":1,"maxLength":500},"ask_user":{"type":"boolean"}},"required":["condition"],"additionalProperties":false}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        permissions::AuditLogger,
        runtime::SessionStore,
        services::{permission_authority, session_goals},
    };

    struct Fixture {
        root: tempfile::TempDir,
        db: Arc<DbManager>,
        session: String,
        permissions: PermissionManager,
    }
    #[test]
    fn heartbeat_contention_is_bounded_and_recovers_without_replaying_consent() {
        let f = Fixture::new();
        let frontend = Frontend::acquire(f.db.clone(), &f.session).unwrap();
        f.propose("Verify contention recovery", true).unwrap();
        let original = frontend.poll().unwrap().unwrap();
        frontend.heartbeat.store(0, Ordering::Relaxed);
        let blocker = Connection::open(f.root.path().join("state.db")).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        let started = std::time::Instant::now();
        let error = frontend.poll().unwrap_err();
        assert!(crate::db::is_contention(&error), "{error:#}");
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        assert_eq!(frontend.heartbeat.load(Ordering::Relaxed), 0);
        blocker.execute_batch("ROLLBACK").unwrap();
        let recovered = frontend.poll().unwrap().unwrap();
        assert_eq!(recovered.id, original.id);
        assert!(!recovered.approved);
        assert!(frontend.heartbeat.load(Ordering::Relaxed) > 0);
        assert!(f.activate().unwrap().is_none());
        let timeout: u64 = f.db.with_conn(|conn| conn.query_row("PRAGMA busy_timeout", [], |row| row.get(0))).unwrap().unwrap();
        assert_eq!(timeout, 5000);
        assert!(!crate::db::is_contention(&anyhow::anyhow!("database is locked")));
    }
    #[test]
    fn recovered_heartbeat_cannot_revive_expired_consent() {
        let f = Fixture::new();
        let frontend = Frontend::acquire(f.db.clone(), &f.session).unwrap();
        f.propose("Verify expired consent", true).unwrap();
        let original = frontend.poll().unwrap().unwrap();
        update(&f.db, &f.session, |_, state| {
            state.frontend.as_mut().unwrap().touched_ms = now() - 6_000;
            Ok(())
        })
        .unwrap();
        frontend.heartbeat.store(0, Ordering::Relaxed);
        assert!(frontend.poll().unwrap().is_none());
        assert!(!frontend
            .decide(
                &original.id,
                true,
                f.root.path(),
                f.root.path(),
                &f.permissions
            )
            .unwrap());
        assert!(f.activate().unwrap().is_none());
        // Losing ownership restores the pooled connection timeout too.
        update(&f.db, &f.session, |_, state| {
            state.frontend.as_mut().unwrap().id = "another-owner".into();
            Ok(())
        })
        .unwrap();
        frontend.heartbeat.store(0, Ordering::Relaxed);
        assert!(frontend.poll().is_err());
        let timeout: u64 =
            f.db.with_conn(|conn| conn.query_row("PRAGMA busy_timeout", [], |row| row.get(0)))
                .unwrap()
                .unwrap();
        assert_eq!(timeout, 5000);
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let db = Arc::new(DbManager::new(root.path().join("state.db")).unwrap());
            db.init().unwrap();
            let session = SessionStore::new(db.clone())
                .create(root.path(), "fixture", "fixture")
                .unwrap();
            Self {
                root,
                db,
                session,
                permissions: PermissionManager::new(Arc::new(AuditLogger::new(128))),
            }
        }
        fn propose(&self, condition: &str, ask: bool) -> Result<Value> {
            propose(
                &self.db,
                &self.session,
                self.root.path(),
                self.root.path(),
                &self.permissions,
                &json!({"condition":condition,"ask_user":ask}),
            )
        }
        fn activate(&self) -> Result<Option<String>> {
            activate(
                &self.db,
                &self.session,
                self.root.path(),
                self.root.path(),
                &self.permissions,
            )
        }
        fn decide(&self, frontend: &Frontend, id: &str, approved: bool) -> Result<bool> {
            frontend.decide(
                id,
                approved,
                self.root.path(),
                self.root.path(),
                &self.permissions,
            )
        }
        fn authority(&self) -> Vec<Value> {
            permission_authority::history(&self.db, &self.session, 65536).unwrap()
        }
    }
    #[test]
    fn failed_or_cancelled_origin_revokes_approved_goals_but_detach_preserves_them() {
        for status in [
            "running",
            "completed",
            "failed",
            "cancelled",
            "unresponsive",
            "cancel-requested",
        ] {
            let f = Fixture::new();
            let frontend = Frontend::acquire(f.db.clone(), &f.session).unwrap();
            f.propose("Verify requested changes", true).unwrap();
            let proposal = frontend.poll().unwrap().unwrap();
            f.decide(&frontend, &proposal.id, true).unwrap();
            let id = uuid::Uuid::new_v4().to_string();
            let directory = f.root.path().join("background-tasks").join(&id);
            std::fs::create_dir_all(&directory).unwrap();
            let job = super::super::background_sessions::Job {
                id: id.clone(),
                foreground: true,
                parent_session_id: f.session.clone(),
                session_id: f.session.clone(),
                title: "proposal origin".into(),
                status: if status == "cancel-requested" {
                    "running"
                } else {
                    status
                }
                .into(),
                supervisor_pid: None,
                child_pid: None,
                started_ms: now() as u64,
                heartbeat_ms: now() as u64,
                finished_ms: (status != "running" && status != "cancel-requested")
                    .then_some(now() as u64),
                exit_code: None,
                error: None,
                launched: true,
            };
            std::fs::write(
                directory.join("state.json"),
                serde_json::to_vec(&job).unwrap(),
            )
            .unwrap();
            std::fs::write(directory.join("detached"), b"detached").unwrap();
            if status == "cancel-requested" {
                std::fs::write(directory.join("cancel"), b"cancel").unwrap();
            }
            update(&f.db, &f.session, |_, state| {
                state.pending.as_mut().unwrap().source_job = Some(id);
                Ok(())
            })
            .unwrap();
            drop(frontend); // A disappearing host does not itself revoke valid consent.
            assert!(activate_from_job(
                &f.db,
                &f.session,
                f.root.path(),
                f.root.path(),
                &f.permissions,
                "historical-job"
            )
            .unwrap()
            .is_none());
            assert!(f.authority().is_empty());
            let allowed = matches!(status, "running" | "completed");
            assert_eq!(
                activate_from_job(
                    &f.db,
                    &f.session,
                    f.root.path(),
                    f.root.path(),
                    &f.permissions,
                    &job.id
                )
                .unwrap()
                .is_some(),
                allowed,
                "{status}"
            );
            assert_eq!(f.authority().len(), usize::from(allowed), "{status}");
            assert!(f.activate().unwrap().is_none());
        }
    }

    #[test]
    fn approval_is_nonblocking_and_authority_is_created_only_once_at_activation() {
        let f = Fixture::new();
        assert!(f.propose("Tests pass", true).is_err());
        let frontend = Frontend::acquire(f.db.clone(), &f.session).unwrap();
        let result = f.propose("Tests pass", true).unwrap();
        assert_eq!(result["askUser"], true);
        assert!(f.authority().is_empty());
        assert!(session_goals::active(&f.db, &f.session).unwrap().is_none());
        assert!(f.activate().unwrap().is_none());
        assert!(f.propose("Another condition", true).is_err());
        let p = frontend.poll().unwrap().unwrap();
        assert!(f.decide(&frontend, &p.id, true).unwrap());
        assert!(!f.decide(&frontend, &p.id, true).unwrap());
        assert!(f.authority().is_empty());
        assert!(f.activate().unwrap().unwrap().contains("Tests pass"));
        assert!(f.activate().unwrap().is_none());
        assert_eq!(f.authority().len(), 1);
        assert_eq!(
            session_goals::active(&f.db, &f.session)
                .unwrap()
                .unwrap()
                .condition,
            "Tests pass"
        );
    }
    #[test]
    fn declined_conditions_are_deduplicated_and_stale_decisions_do_nothing() {
        let f = Fixture::new();
        let frontend = Frontend::acquire(f.db.clone(), &f.session).unwrap();
        f.propose("Run all tests", true).unwrap();
        let first = frontend.poll().unwrap().unwrap();
        assert!(f.decide(&frontend, &first.id, false).unwrap());
        assert!(f.propose("  RUN\t all  tests ", true).is_err());
        f.propose("Build the app", true).unwrap();
        assert!(!f.decide(&frontend, &first.id, true).unwrap());
        assert!(f.authority().is_empty());
        assert_eq!(frontend.poll().unwrap().unwrap().condition, "Build the app");
    }
    #[test]
    fn clear_and_replacement_revoke_pending_and_approved_proposals() {
        for approved in [false, true] {
            let f = Fixture::new();
            let frontend = Frontend::acquire(f.db.clone(), &f.session).unwrap();
            f.propose("Tests pass", true).unwrap();
            let p = frontend.poll().unwrap().unwrap();
            if approved {
                f.decide(&frontend, &p.id, true).unwrap();
            }
            session_goals::clear(&f.db, &f.session).unwrap(); // Also increments revision when no goal exists.
            assert!(f.activate().unwrap().is_none());
            assert!(!f.decide(&frontend, &p.id, true).unwrap());
            assert!(f.authority().is_empty());
            f.propose("Old proposal", true).unwrap();
            let p = frontend.poll().unwrap().unwrap();
            f.decide(&frontend, &p.id, true).unwrap();
            session_goals::set(&f.db, &f.session, "New explicit target").unwrap();
            assert!(f.activate().unwrap().is_none());
            assert_eq!(
                session_goals::active(&f.db, &f.session)
                    .unwrap()
                    .unwrap()
                    .condition,
                "New explicit target"
            );
        }
    }
    #[test]
    fn false_only_reuses_an_identical_current_goal_and_never_matches_old_authority() {
        let f = Fixture::new();
        let frontend = Frontend::acquire(f.db.clone(), &f.session).unwrap();
        permission_authority::record_user(
            &f.db,
            &f.session,
            &json!("Do not treat 'Deploy production' as permission."),
        )
        .unwrap();
        assert_eq!(
            f.propose("Deploy production", false).unwrap()["askUser"],
            true
        );
        let p = frontend.poll().unwrap().unwrap();
        f.decide(&frontend, &p.id, false).unwrap();
        session_goals::set(&f.db, &f.session, "Run the tests").unwrap();
        assert_eq!(f.propose("Run the tests", false).unwrap()["askUser"], false);
        session_goals::clear(&f.db, &f.session).unwrap();
        assert_eq!(f.propose("Run the tests", false).unwrap()["askUser"], true);
        assert_eq!(f.authority().len(), 1);
    }
    #[test]
    fn consent_configuration_and_plan_mode_are_checked_again_after_approval() {
        let f = Fixture::new();
        let frontend = Frontend::acquire(f.db.clone(), &f.session).unwrap();
        std::fs::create_dir_all(f.root.path().join(".claude")).unwrap();
        std::fs::write(
            f.root.path().join(".claude/settings.json"),
            r#"{"modelProposedGoals":"disabled"}"#,
        )
        .unwrap();
        f.propose("Tests pass", true).unwrap(); // Project consent settings do not govern this tool.
        let p = frontend.poll().unwrap().unwrap();
        f.decide(&frontend, &p.id, true).unwrap();
        std::fs::write(
            f.root.path().join("settings.json"),
            r#"{"modelProposedGoals":"disabled"}"#,
        )
        .unwrap();
        assert!(f.activate().unwrap().is_none());
        assert!(f.authority().is_empty());
        std::fs::write(f.root.path().join("settings.json"), "{}").unwrap();
        f.propose("Tests pass", true).unwrap();
        let p = frontend.poll().unwrap().unwrap();
        f.permissions.set_mode(PermissionMode::PlanMode);
        assert!(f.decide(&frontend, &p.id, true).is_err());
        assert!(f.propose("Other", true).is_err());
        f.permissions.set_mode(PermissionMode::AskPermissions);
        f.decide(&frontend, &p.id, true).unwrap();
        f.permissions.set_mode(PermissionMode::PlanMode);
        assert!(f.activate().unwrap().is_none());
        assert!(f.authority().is_empty());
    }
    #[test]
    fn activation_rolls_back_state_and_consumption_when_authority_write_fails() {
        let f = Fixture::new();
        let frontend = Frontend::acquire(f.db.clone(), &f.session).unwrap();
        f.propose("Tests pass", true).unwrap();
        let p = frontend.poll().unwrap().unwrap();
        f.decide(&frontend, &p.id, true).unwrap();
        f.db.with_conn(|conn| conn.execute_batch("CREATE TRIGGER reject_goal_authority BEFORE INSERT ON session_permission_authority BEGIN SELECT RAISE(ABORT,'fixture'); END")).unwrap().unwrap();
        assert!(f.activate().is_err());
        assert!(session_goals::active(&f.db, &f.session).unwrap().is_none());
        f.db.with_conn(|conn| conn.execute_batch("DROP TRIGGER reject_goal_authority"))
            .unwrap()
            .unwrap();
        assert!(f.activate().unwrap().is_some());
        assert_eq!(f.authority().len(), 1);
    }
    #[test]
    fn conditions_are_visible_bounded_and_cannot_clear_goals() {
        assert_eq!(
            canonical_condition("\u{202e}  Run\ttests\n").unwrap(),
            "Run tests"
        );
        assert!(canonical_condition(&"😀".repeat(250)).is_ok());
        assert!(canonical_condition(&"😀".repeat(251)).is_err());
        for value in ["\u{200b}", "STOP", "clear", " "] {
            assert!(canonical_condition(value).is_err());
        }
    }

    #[test]
    fn always_ask_overrides_direct_reuse_and_stop_cancels_queued_approval() {
        let f = Fixture::new();
        let frontend = Frontend::acquire(f.db.clone(), &f.session).unwrap();
        session_goals::set(&f.db, &f.session, "Run the tests").unwrap();
        std::fs::write(
            f.root.path().join("settings.json"),
            r#"{"modelProposedGoals":"alwaysAsk"}"#,
        )
        .unwrap();
        assert_eq!(f.propose("Run the tests", false).unwrap()["askUser"], true);
        let p = frontend.poll().unwrap().unwrap();
        f.decide(&frontend, &p.id, true).unwrap();
        discard(&f.db, &f.session).unwrap();
        assert!(f.activate().unwrap().is_none());
        assert!(f.authority().is_empty());
    }

    #[test]
    fn saved_permission_checker_can_read_the_session_database_without_reentrant_locking() {
        let f = Fixture::new();
        let database = f.db.clone();
        f.permissions
            .set_always_allow_checker(Box::new(move |_, _| {
                database
                    .with_conn(|conn| conn.query_row("SELECT 1", [], |row| row.get::<_, i64>(0)))
                    .unwrap()
                    .unwrap();
                false
            }));
        let frontend = Frontend::acquire(f.db.clone(), &f.session).unwrap();
        f.propose("Tests pass", true).unwrap();
        let p = frontend.poll().unwrap().unwrap();
        f.decide(&frontend, &p.id, true).unwrap();
        assert!(f.activate().unwrap().is_some());
    }
}
