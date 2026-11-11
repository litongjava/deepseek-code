//! Durable session role: a session is a normal Agent unless the user makes it a
//! Router. The role never changes the permission mode.
use crate::db::DbManager;
use anyhow::{bail, Result};
use rusqlite::OptionalExtension;
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

fn key(session: &str) -> String {
    format!("super-agent.role.{session}")
}

pub fn load(db: &DbManager, session: &str) -> Result<Role> {
    let value: Option<String> = db.with_conn(|conn| {
        conn.query_row(
            "SELECT value FROM peripheral_settings WHERE key=?1",
            [key(session)],
            |row| row.get(0),
        )
        .optional()
    })??;
    match value.as_deref() {
        None | Some("") => Ok(Role::Agent),
        Some(value) => Role::parse(value),
    }
}

pub fn set(db: &DbManager, session: &str, role: Role) -> Result<()> {
    anyhow::ensure!(!session.trim().is_empty(), "Session is required");
    db.with_conn(|conn| {
        conn.execute(
            "INSERT INTO peripheral_settings(key,value) VALUES(?1,?2)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [key(session), role.as_str().to_owned()],
        )
    })??;
    Ok(())
}

/// The task whose current coder or reviewer is this session, if any. This is
/// the only way a member tool grants access; the model cannot claim membership.
pub fn membership(db: &DbManager, session: &str) -> Result<Option<(String, MemberRole)>> {
    db.with_conn(|conn| {
        let row: Option<(String, Option<String>, Option<String>)> = conn
            .query_row(
                "SELECT task_id, coder_session_id, reviewer_session_id FROM sa_tasks WHERE coder_session_id=?1 OR reviewer_session_id=?1 ORDER BY updated_at DESC LIMIT 1",
                [session],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        Ok(row.and_then(|(task_id, coder, reviewer)| {
            if coder.as_deref() == Some(session) {
                Some((task_id, MemberRole::Coder))
            } else if reviewer.as_deref() == Some(session) {
                Some((task_id, MemberRole::Reviewer))
            } else {
                None
            }
        }))
    })?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::RuntimePaths;

    fn fixture() -> (tempfile::TempDir, std::sync::Arc<DbManager>) {
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
        let (temp, db) = fixture();
        let data = temp.path().join("data");
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
    }
}
