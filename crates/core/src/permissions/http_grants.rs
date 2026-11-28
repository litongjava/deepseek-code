//! Plaintext HTTP approvals are user-local state. Never import these from project settings.
use anyhow::{ensure, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantScope {
    Session,
    Project,
    User,
}

impl GrantScope {
    fn name(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Project => "project",
            Self::User => "user",
        }
    }
}

#[derive(Debug, Serialize)]
pub struct GrantRecord {
    pub id: i64,
    pub origin: String,
    pub scope: String,
    pub project: String,
    pub session: String,
}

fn store(data_dir: &Path) -> Result<Connection> {
    ensure!(
        data_dir.is_absolute(),
        "expected absolute application data directory"
    );
    let dir = data_dir.join("state");
    std::fs::create_dir_all(&dir)?;
    let conn = Connection::open(dir.join("http-grants.db"))?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS grants (
        id INTEGER PRIMARY KEY AUTOINCREMENT, origin TEXT NOT NULL, scope TEXT NOT NULL,
        project TEXT NOT NULL, session TEXT NOT NULL, UNIQUE(origin, scope, project, session))",
    )?;
    Ok(conn)
}

fn identity(project: &Path, origin: &str) -> Result<String> {
    let url = reqwest::Url::parse(origin)?;
    ensure!(
        url.scheme() == "http"
            && !origin.contains('*')
            && url.origin().ascii_serialization() == origin,
        "expected exact canonical HTTP origin"
    );
    ensure!(project.is_absolute(), "missing trusted workspace identity");
    let path = project
        .canonicalize()
        .context("resolve HTTP grant workspace")?;
    ensure!(path.is_dir(), "workspace is not a directory");
    Ok(path
        .to_str()
        .context("workspace path is not Unicode")?
        .to_owned())
}

/// The session identity is the trusted incarnation, not a reusable conversation ID.
pub fn grant(
    data_dir: &Path,
    project: &Path,
    session: &str,
    origin: &str,
    scope: GrantScope,
) -> Result<()> {
    let project = identity(project, origin)?;
    ensure!(
        scope != GrantScope::Session || !session.is_empty(),
        "missing trusted session incarnation"
    );
    let project = if scope == GrantScope::User {
        ""
    } else {
        &project
    };
    let session = if scope == GrantScope::Session {
        session
    } else {
        ""
    };
    store(data_dir)?.execute(
        "INSERT INTO grants(origin, scope, project, session) VALUES (?1,?2,?3,?4)
        ON CONFLICT(origin,scope,project,session) DO NOTHING",
        params![origin, scope.name(), project, session],
    )?;
    Ok(())
}

pub fn lookup(
    data_dir: &Path,
    project: &Path,
    session: &str,
    origin: &str,
) -> Result<Option<GrantScope>> {
    let project = identity(project, origin)?;
    let scope: Option<String> = store(data_dir)?.query_row("SELECT scope FROM grants WHERE origin=?1 AND (
        (scope='session' AND project=?2 AND session=?3 AND session<>'') OR
        (scope='project' AND project=?2 AND session='') OR
        (scope='user' AND project='' AND session='')) ORDER BY CASE scope WHEN 'session' THEN 0 WHEN 'project' THEN 1 ELSE 2 END LIMIT 1",
        params![origin,project,session], |row| row.get(0)).optional()?;
    Ok(scope.map(|s| match s.as_str() {
        "session" => GrantScope::Session,
        "project" => GrantScope::Project,
        "user" => GrantScope::User,
        _ => unreachable!(),
    }))
}

pub fn list(data_dir: &Path) -> Result<Vec<GrantRecord>> {
    let conn = store(data_dir)?;
    let mut stmt =
        conn.prepare("SELECT id, origin, scope, project, session FROM grants ORDER BY id")?;
    let rows = stmt.query_map([], |row| {
        Ok(GrantRecord {
            id: row.get(0)?,
            origin: row.get(1)?,
            scope: row.get(2)?,
            project: row.get(3)?,
            session: row.get(4)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

pub fn revoke(data_dir: &Path, id: i64) -> Result<bool> {
    Ok(store(data_dir)?.execute("DELETE FROM grants WHERE id=?1", [id])? != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use GrantScope::*;
    const ORIGIN: &str = "http://example.org:8080";

    #[test]
    fn scopes_survive_reopen_and_are_isolated_by_identity() -> Result<()> {
        let data = tempfile::tempdir()?;
        let other_user = tempfile::tempdir()?;
        let a = tempfile::tempdir()?;
        let b = tempfile::tempdir()?;
        let (d, a, b) = (data.path(), a.path(), b.path());
        let get = |p: &Path, s| lookup(d, p, s, ORIGIN);
        grant(d, a, "first", ORIGIN, Session)?;
        assert_eq!(get(a, "first")?, Some(Session));
        assert_eq!(get(a, "")?, None);
        assert!(grant(d, a, "", ORIGIN, Session).is_err());
        assert_eq!(get(a, "recreated")?, None);
        assert_eq!(get(b, "first")?, None);
        grant(d, a, "first", ORIGIN, Project)?;
        assert_eq!(get(a, "recreated")?, Some(Project));
        assert_eq!(get(a, "")?, Some(Project));
        assert_eq!(get(b, "second")?, None);
        assert!(a.read_dir()?.next().is_none());
        grant(d, b, "second", ORIGIN, User)?;
        assert_eq!(get(b, "first")?, Some(User));
        assert_eq!(lookup(other_user.path(), a, "first", ORIGIN)?, None);
        for wrong in ["http://example.org", "http://sub.example.org:8080"] {
            assert_eq!(lookup(d, a, "first", wrong)?, None);
        }
        for invalid in [
            "http://u@example.org",
            "http://example.org/x",
            "https://example.org",
            "http://*.org",
        ] {
            assert!(grant(d, a, "first", invalid, User).is_err());
        }
        for (scope, next) in [
            (Session, Some(Project)),
            (Project, Some(User)),
            (User, None),
        ] {
            let id = list(d)?
                .into_iter()
                .find(|row| row.scope == scope.name())
                .unwrap()
                .id;
            assert!(revoke(d, id)?);
            assert!(!revoke(d, id)?);
            assert_eq!(get(a, "first")?, next);
        }
        Ok(())
    }

    #[test]
    fn concurrent_connections_preserve_independent_and_duplicate_grants() -> Result<()> {
        let data = tempfile::tempdir()?;
        let project = tempfile::tempdir()?;
        let barrier = std::sync::Barrier::new(8);
        std::thread::scope(|threads| {
            for i in 0..8 {
                let (b, d, p) = (&barrier, data.path(), project.path());
                threads.spawn(move || {
                    b.wait();
                    grant(d, p, &i.to_string(), ORIGIN, Session).unwrap();
                    grant(d, p, &i.to_string(), ORIGIN, Project).unwrap();
                });
            }
        });
        assert_eq!(list(data.path())?.len(), 9);
        Ok(())
    }
}
