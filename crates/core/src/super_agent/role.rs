//! Durable session role: a session is a normal Agent unless the user makes it a
//! Router. The role never changes the permission mode.
#[cfg(test)]
use crate::db::DbManager;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// A regular Agent session, which is what every existing session is.
    #[default]
    Agent,
    /// A Super Agent Router: it schedules work and does not edit business code.
    Router,
}

impl Role {
    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "agent" => Ok(Self::Agent),
            "router" => Ok(Self::Router),
            other => bail!("Unknown role: {other}. Use agent or router"),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Router => "router",
        }
    }

    pub fn is_router(self) -> bool {
        self == Self::Router
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Role for one task member session. Membership is recorded by the task, not
/// chosen by the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemberRole {
    Coder,
    Reviewer,
}

impl MemberRole {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Coder => "coder",
            Self::Reviewer => "reviewer",
        }
    }
}

pub fn load(db: &impl crate::db::session_store::SessionAccess, session: &str) -> Result<Role> {
    db.select_session(session)?.role()
}

pub fn set(
    db: &impl crate::db::session_store::SessionAccess,
    session: &str,
    role: Role,
) -> Result<()> {
    anyhow::ensure!(!session.trim().is_empty(), "Session is required");
    db.select_session(session)?
        .set_role(role)
        .context("Saving session role")
}

/// Only the task directory can grant a member tool access to a task.
pub fn membership(
    db: &impl crate::db::session_store::TaskMembership,
    session: &str,
) -> Result<Option<(String, MemberRole)>> {
    db.member_session(session)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::RuntimePaths;

    fn fixture() -> (tempfile::TempDir, std::sync::Arc<crate::db::session_store::Stores>) {
        let temp = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(temp.path().join("data")).unwrap();
        let db = paths.open_database().unwrap();
        (temp, db)
    }

    #[test]
    fn role_defaults_to_agent_and_is_durable_per_session() {
        let (temp, db) = fixture();
        let sessions = crate::runtime::SessionStore::new(db.clone());
        let first = sessions.create(temp.path(), "fixture", "fixture").unwrap();
        let second = sessions.create(temp.path(), "fixture", "fixture").unwrap();
        assert_eq!(load(&db, &first).unwrap(), Role::Agent);
        set(&db, &first, Role::Router).unwrap();
        assert_eq!(load(&db, &first).unwrap(), Role::Router);
        assert_eq!(load(&db, &second).unwrap(), Role::Agent);
        // Reopening the database keeps the persisted role.
        let reopened = RuntimePaths::new(temp.path().join("data"))
            .unwrap()
            .open_database()
            .unwrap();
        assert_eq!(load(&reopened, &first).unwrap(), Role::Router);
        set(&db, &first, Role::Agent).unwrap();
        assert_eq!(load(&db, &first).unwrap(), Role::Agent);
        assert!(Role::parse("router").unwrap().is_router());
        assert!(Role::parse("supervisor").is_err());
        assert!(set(&db, "  ", Role::Router).is_err());
    }

    #[test]
    fn membership_comes_from_the_task_record_only() {
        let temp = tempfile::tempdir().unwrap();
        let data = temp.path().join("data");
        let db = std::sync::Arc::new(crate::db::session_store::Stores::open(&data).unwrap());
        let store = crate::super_agent::TaskStore::new(db.clone(), data.clone());
        let sessions = crate::runtime::SessionStore::new(db.clone());
        let router = sessions.create(temp.path(), "fixture", "fixture").unwrap();
        let coder = sessions.create(temp.path(), "fixture", "fixture").unwrap();
        let outsider = sessions.create(temp.path(), "fixture", "fixture").unwrap();
        let spec = crate::super_agent::TaskSpec {
            title: "Member".into(),
            workspace: temp.path().to_path_buf(),
            targets: vec![temp.path().to_string_lossy().into_owned()],
            goal: "test".into(),
            requirements: vec![],
            acceptance_criteria: vec![],
            constraints: vec![],
            out_of_scope: vec![],
            target_branch: None,
            repository_target_branches: Default::default(),
        };
        let task = store.create_task(&router, spec, "create").unwrap();
        store
            .set_sessions(&task.task_id, Some(&coder), None)
            .unwrap();
        assert_eq!(
            membership(&db, &coder).unwrap(),
            Some((task.task_id.clone(), MemberRole::Coder))
        );
        assert_eq!(membership(&db, &outsider).unwrap(), None);
        assert_eq!(membership(&db, &router).unwrap(), None);
        set(&db, &coder, Role::Router).unwrap();
        assert_eq!(load(&db, &coder).unwrap(), Role::Router);
        db.open_session(&coder).unwrap().delete().unwrap();
        db.create_code_session(temp.path(), "fixture", "fixture", &coder)
            .unwrap();
        assert_eq!(membership(&db, &coder).unwrap(), None);
        assert_eq!(load(&db, &coder).unwrap(), Role::Agent);
    }

    #[test]
    fn set_waits_for_another_writer_instead_of_failing() {
        use std::time::{Duration, Instant};
        let (_temp, db) = fixture();
        let session = uuid::Uuid::new_v4().to_string();
        db.create(&session, Default::default()).unwrap();
        let path = db
            .data_dir()
            .join("data/sessions")
            .join(&session)
            .join("session.db");
        let (holder, release) = crate::db::test_support::hold_write_lock(path);
        let releaser =
            crate::db::test_support::release_after(release, holder, Duration::from_secs(6));
        let started = Instant::now();
        set(&db, &session, Role::Router).unwrap();
        assert!(
            started.elapsed() >= Duration::from_secs(5),
            "set returned after {:?}",
            started.elapsed()
        );
        releaser.join().unwrap();
        assert_eq!(load(&db, &session).unwrap(), Role::Router);
    }
}
