//! Explicit session goals. All mutations are transactional; stale evaluations
//! cannot recreate a cleared or replaced goal.
use crate::db::DbManager;
use anyhow::{ensure, Result};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Goal {
    pub id: String,
    pub condition: String,
    pub state: String,
    pub reason: Option<String>,
    pub started_ms: i64,
    pub finished_ms: Option<i64>,
    pub turns: u64,
    pub tokens: u64,
    #[serde(default)]
    pub unreported_requests: u64,
    #[serde(default)]
    pub model_requests: u64,
    #[serde(default)]
    pub run_start_requests: u64,
    pub stop_blocks: u64,
    #[serde(default)]
    pub retry_attempts: u8,
    #[serde(default)]
    pub retry_after_ms: Option<i64>,
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    active: Option<Goal>,
    last: Option<Goal>,
    #[serde(default)]
    revision: u64,
}

fn modify<T>(db: &DbManager, session: &str, f: impl FnOnce(&mut State) -> T) -> Result<T> {
    modify_with_conn(db, session, |_, state| Ok(f(state)))
}

fn modify_with_conn<T>(
    db: &DbManager,
    session: &str,
    f: impl FnOnce(&rusqlite::Connection, &mut State) -> Result<T>,
) -> Result<T> {
    db.with_conn(|conn| -> Result<T> {
        let tx =
            rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
        let result = modify_conn(&tx, session, f)?;
        tx.commit()?;
        Ok(result)
    })?
}

fn read_conn(conn: &rusqlite::Connection, session: &str) -> Result<State> {
    let json: Option<String> = conn
        .query_row(
            "SELECT value FROM peripheral_settings WHERE key=?1",
            [format!("session.goal.{session}")],
            |r| r.get(0),
        )
        .optional()?;
    Ok(json
        .map(|s| serde_json::from_str(&s))
        .transpose()?
        .unwrap_or_default())
}

fn modify_conn<T>(
    conn: &rusqlite::Connection,
    session: &str,
    f: impl FnOnce(&rusqlite::Connection, &mut State) -> Result<T>,
) -> Result<T> {
    let mut state = read_conn(conn, session)?;
    let result = f(conn, &mut state)?;
    conn.execute("INSERT INTO peripheral_settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [format!("session.goal.{session}"), serde_json::to_string(&state)?])?;
    Ok(result)
}

/// Explicit goal replacement/cancellation revision, for transactional proposal CAS.
pub fn revision_conn(conn: &rusqlite::Connection, session: &str) -> Result<u64> {
    Ok(read_conn(conn, session)?.revision)
}

pub fn active(db: &DbManager, session: &str) -> Result<Option<Goal>> {
    // Read without acquiring a write transaction: evaluator polls this while
    // another process may be clearing the goal.
    db.with_conn(|conn| -> Result<Option<Goal>> {
        let json: Option<String> = conn
            .query_row(
                "SELECT value FROM peripheral_settings WHERE key=?1",
                [format!("session.goal.{session}")],
                |r| r.get(0),
            )
            .optional()?;
        Ok(json
            .map(|s| serde_json::from_str::<State>(&s))
            .transpose()?
            .and_then(|s| s.active))
    })?
}

pub fn status(db: &DbManager, session: &str) -> Result<Option<Goal>> {
    modify(db, session, |s| s.active.clone().or_else(|| s.last.clone()))
}

pub fn set(db: &DbManager, session: &str, condition: &str) -> Result<Goal> {
    let goal = new_goal(condition)?;
    modify(db, session, |s| replace(s, &goal))?;
    Ok(goal)
}

/// The caller must hold an immediate transaction and validate the proposal's
/// approval/revision before calling. This function does not grant authority.
pub fn set_conn(conn: &rusqlite::Connection, session: &str, condition: &str) -> Result<Goal> {
    let goal = new_goal(condition)?;
    modify_conn(conn, session, |_, s| {
        replace(s, &goal);
        Ok(())
    })?;
    Ok(goal)
}

pub fn set_from_user(
    db: &DbManager,
    session: &str,
    condition: &str,
    raw_input: &str,
) -> Result<Goal> {
    let goal = new_goal(condition)?;
    modify_with_conn(db, session, |conn, state| {
        super::permission_authority::record_conn(
            conn,
            session,
            super::permission_authority::Source::User,
            &serde_json::json!(raw_input),
        )?;
        replace(state, &goal);
        Ok(())
    })?;
    Ok(goal)
}

fn replace(state: &mut State, goal: &Goal) {
    state.revision = state.revision.saturating_add(1);
    state.active = Some(goal.clone());
}

fn new_goal(condition: &str) -> Result<Goal> {
    let condition = condition.trim();
    ensure!(
        !condition.is_empty() && condition.encode_utf16().count() <= 4000,
        "Goal must contain 1 to 4000 UTF-16 code units"
    );
    Ok(Goal {
        id: uuid::Uuid::new_v4().to_string(),
        condition: condition.into(),
        state: "active".into(),
        reason: None,
        started_ms: chrono::Utc::now().timestamp_millis(),
        finished_ms: None,
        turns: 0,
        tokens: 0,
        unreported_requests: 0,
        model_requests: 0,
        run_start_requests: 0,
        stop_blocks: 0,
        retry_attempts: 0,
        retry_after_ms: None,
    })
}

pub fn clear(db: &DbManager, session: &str) -> Result<bool> {
    modify(db, session, |s| {
        s.revision = s.revision.saturating_add(1);
        s.active.take().is_some()
    })
}

/// Host-only boundary for an explicit human cancellation. Persist revocation
/// with the state change so automatic permission review cannot retain only
/// the earlier goal after cancellation. Internal cleanup must use `clear`.
pub fn clear_from_user(db: &DbManager, session: &str, raw_input: &str) -> Result<bool> {
    modify_with_conn(db, session, |conn, state| {
        super::permission_authority::record_conn(
            conn,
            session,
            super::permission_authority::Source::User,
            &serde_json::json!(raw_input),
        )?;
        state.revision = state.revision.saturating_add(1);
        Ok(state.active.take().is_some())
    })
}

/// Only a real user input resets the shared Stop-hook continuation budget.
pub fn begin_input(db: &DbManager, session: &str) -> Result<()> {
    modify(db, session, |s| {
        if let Some(g) = &mut s.active {
            g.id = uuid::Uuid::new_v4().to_string();
            g.state = "active".into();
            g.stop_blocks = 0;
            g.retry_attempts = 0;
            g.retry_after_ms = None;
            g.run_start_requests = g.model_requests;
        }
    })
}

pub fn pause(db: &DbManager, session: &str, id: &str, reason: &str) -> Result<bool> {
    modify(db, session, |s| {
        let Some(g) = s.active.as_mut().filter(|g| g.id == id) else {
            return false;
        };
        g.state = "paused".into();
        g.reason = Some(reason.into());
        g.retry_after_ms = None;
        true
    })
}

pub fn record_turn(
    db: &DbManager,
    session: &str,
    id: &str,
    tokens: u64,
    requests: u64,
    unreported: u64,
) -> Result<()> {
    modify(db, session, |s| {
        if let Some(g) = s.active.as_mut().filter(|g| g.id == id) {
            g.turns += 1;
            g.tokens = g.tokens.saturating_add(tokens);
            g.model_requests = g.model_requests.saturating_add(requests);
            g.unreported_requests = g.unreported_requests.saturating_add(unreported);
        }
    })
}

pub fn verdict(db: &DbManager, session: &str, id: &str, state: &str, reason: &str) -> Result<bool> {
    modify(db, session, |s| {
        let Some(g) = s
            .active
            .as_mut()
            .filter(|g| g.id == id && g.state == "active")
        else {
            return false;
        };
        g.reason = Some(reason.into());
        if state != "continue" {
            g.state = state.into();
            g.finished_ms = Some(chrono::Utc::now().timestamp_millis());
            s.last = s.active.take();
        }
        true
    })
}

pub fn stop_cap() -> u64 {
    std::env::var("CLAUDE_CODE_STOP_HOOK_BLOCK_CAP")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8)
}

pub fn within_stop_cap(blocks: u64, cap: u64) -> bool {
    cap == 0 || blocks <= cap
}

fn retry_delay_ms(attempt: u8, jitter_per_mille: u16) -> Option<i64> {
    let base = *[60_000_i64, 300_000, 900_000].get(usize::from(attempt))?;
    Some(base + base * i64::from(jitter_per_mille.min(200)) / 1000)
}

/// Persist before announcing a retry, so another frontend can interrupt it.
pub fn arm_retry(db: &DbManager, session: &str, id: &str, reason: &str) -> Result<Option<Goal>> {
    let bytes = uuid::Uuid::new_v4();
    let jitter = u16::from_le_bytes([bytes.as_bytes()[0], bytes.as_bytes()[1]]) % 201;
    modify(db, session, |s| {
        let g = s
            .active
            .as_mut()
            .filter(|g| g.id == id && g.state == "active")?;
        let delay = retry_delay_ms(g.retry_attempts, jitter)?;
        g.retry_attempts += 1;
        g.retry_after_ms = Some(chrono::Utc::now().timestamp_millis() + delay);
        g.reason = Some(reason.into());
        g.state = "waiting".into();
        Some(g.clone())
    })
}

pub fn retry_ready(db: &DbManager, session: &str, id: &str, now_ms: i64) -> Result<bool> {
    modify(db, session, |s| {
        let Some(g) = s
            .active
            .as_mut()
            .filter(|g| g.id == id && g.state == "waiting")
        else {
            return false;
        };
        if !g.retry_after_ms.is_some_and(|at| at <= now_ms) {
            return false;
        }
        g.state = "active".into();
        g.retry_after_ms = None;
        true
    })
}

/// Returns None when there is no active goal: ordinary Stop hooks then use
/// their executor-local budget. Child sessions never consume their parent's.
pub fn block(db: &DbManager, session: &str, id: Option<&str>, cap: u64) -> Result<Option<bool>> {
    modify(db, session, |s| {
        let g = s.active.as_mut()?;
        if id.is_some_and(|id| id != g.id) {
            return Some(false);
        }
        if g.state != "active" {
            return Some(false);
        }
        g.stop_blocks = g.stop_blocks.saturating_add(1);
        let allowed = within_stop_cap(g.stop_blocks, cap);
        if !allowed {
            g.state = "paused".into();
            g.reason = Some("Stop checks capped; send a message to continue".into());
        }
        Some(allowed)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn db() -> (tempfile::TempDir, DbManager) {
        let t = tempfile::tempdir().unwrap();
        let db = DbManager::new(t.path().join("test.db")).unwrap();
        db.init().unwrap();
        (t, db)
    }
    #[test]
    fn stale_verdict_cannot_revive_clear_or_replacement() {
        let (_t, db) = db();
        let a = set(&db, "s", "first").unwrap();
        clear(&db, "s").unwrap();
        assert!(!verdict(&db, "s", &a.id, "achieved", "late").unwrap());
        let b = set(&db, "s", "second").unwrap();
        assert!(!verdict(&db, "s", &a.id, "achieved", "late").unwrap());
        assert_eq!(active(&db, "s").unwrap().unwrap().id, b.id);
    }

    #[test]
    fn explicit_clear_records_revocation_atomically_and_internal_cleanup_has_no_authority() {
        use super::super::permission_authority::{history, record_user};
        use serde_json::json;
        let (root, db) = db();
        let db = std::sync::Arc::new(db);
        let session = crate::runtime::SessionStore::new(db.clone())
            .create(root.path(), "fixture", "fixture")
            .unwrap();
        let goal = set(&db, &session, "Run the tests").unwrap();
        assert!(history(&db, &session, 8192).unwrap().is_empty());
        record_user(&db, &session, &json!("/goal Run the tests")).unwrap();
        db.with_conn(|conn| conn.execute_batch("CREATE TRIGGER reject_goal_revocation BEFORE INSERT ON session_permission_authority BEGIN SELECT RAISE(ABORT,'fixture failure'); END;"))
            .unwrap().unwrap();
        assert!(clear_from_user(&db, &session, "/goal clear").is_err());
        assert!(set_from_user(&db, &session, "replacement", "/goal replacement").is_err());
        assert_eq!(active(&db, &session).unwrap().unwrap().id, goal.id);
        assert_eq!(
            db.with_conn(|conn| revision_conn(conn, &session))
                .unwrap()
                .unwrap(),
            1
        );
        assert_eq!(history(&db, &session, 8192).unwrap().len(), 1);
        db.with_conn(|conn| conn.execute_batch("DROP TRIGGER reject_goal_revocation"))
            .unwrap()
            .unwrap();
        assert!(clear_from_user(&db, &session, "/goal CLEAR").unwrap());
        assert!(active(&db, &session).unwrap().is_none());
        let expected = vec![
            json!({"role":"user","content":"/goal Run the tests"}),
            json!({"role":"user","content":"/goal CLEAR"}),
        ];
        assert_eq!(history(&db, &session, 8192).unwrap(), expected);
        set(&db, &session, "internal restoration").unwrap();
        clear(&db, &session).unwrap();
        assert_eq!(history(&db, &session, 8192).unwrap(), expected);
        // A cancellation still expresses current user intent if a concurrent
        // evaluator already completed the active goal.
        assert!(!clear_from_user(&db, &session, "/goal stop").unwrap());
        assert_eq!(history(&db, &session, 8192).unwrap().len(), 3);
        assert_eq!(
            db.with_conn(|conn| revision_conn(conn, &session))
                .unwrap()
                .unwrap(),
            5
        );
        set_from_user(&db, &session, "Run tests only", "/goal Run tests only").unwrap();
        assert_eq!(
            history(&db, &session, 8192).unwrap().last().unwrap()["content"],
            "/goal Run tests only"
        );
    }

    #[test]
    fn proposal_revision_changes_only_on_explicit_set_or_clear_and_rolls_back() {
        let (_root, db) = db();
        let goal = set(&db, "s", "first").unwrap();
        record_turn(&db, "s", &goal.id, 7, 1, 0).unwrap();
        verdict(&db, "s", &goal.id, "continue", "still working").unwrap();
        begin_input(&db, "s").unwrap();
        db.with_conn(|conn| -> Result<()> {
            assert_eq!(revision_conn(conn, "s")?, 1);
            let tx = crate::db::write_transaction(&conn)?;
            set_conn(&tx, "s", "approved proposal")?;
            assert_eq!(revision_conn(&tx, "s")?, 2);
            tx.rollback()?;
            assert_eq!(revision_conn(conn, "s")?, 1);
            Ok(())
        })
        .unwrap()
        .unwrap();
        assert_eq!(active(&db, "s").unwrap().unwrap().condition, "first");
    }
    #[test]
    fn shared_budget_exceeds_three_then_pauses_and_resumes_without_affecting_children() {
        let (_t, db) = db();
        let g = set(&db, "s", "goal").unwrap();
        set(&db, "child", "child goal").unwrap();
        for n in 1..=8 {
            assert_eq!(
                block(
                    &db,
                    "s",
                    if n % 2 == 0 {
                        Some(g.id.as_str())
                    } else {
                        None
                    },
                    8
                )
                .unwrap(),
                Some(true)
            );
        }
        assert_eq!(block(&db, "s", Some(&g.id), 8).unwrap(), Some(false));
        assert_eq!(active(&db, "child").unwrap().unwrap().stop_blocks, 0);
        begin_input(&db, "s").unwrap();
        assert_eq!(active(&db, "s").unwrap().unwrap().stop_blocks, 0);
        let resumed = active(&db, "s").unwrap().unwrap();
        assert!(!verdict(&db, "s", &g.id, "achieved", "late").unwrap());
        assert_eq!(block(&db, "s", Some(&resumed.id), 8).unwrap(), Some(true));
    }
    #[test]
    fn completion_retains_metrics_but_clears_active() {
        let (_t, db) = db();
        let g = set(&db, "s", "goal").unwrap();
        record_turn(&db, "s", &g.id, 123, 2, 1).unwrap();
        assert!(verdict(&db, "s", &g.id, "achieved", "verified").unwrap());
        assert!(active(&db, "s").unwrap().is_none());
        let g = status(&db, "s").unwrap().unwrap();
        assert_eq!((g.turns, g.tokens), (1, 123));
        assert_eq!(g.unreported_requests, 1);
        assert!(g.finished_ms.is_some());
    }
    #[test]
    fn condition_limit_matches_javascript_utf16_length() {
        let (_t, db) = db();
        assert!(set(&db, "s", &"\u{1f600}".repeat(2000)).is_ok());
        assert!(set(&db, "s", &"\u{1f600}".repeat(2001)).is_err());
        assert!(within_stop_cap(u64::MAX, 0));
    }

    #[test]
    fn transient_retries_follow_original_backoff_and_remain_cancellable() {
        assert_eq!(retry_delay_ms(0, 0), Some(60_000));
        assert_eq!(retry_delay_ms(1, 200), Some(360_000));
        assert_eq!(retry_delay_ms(2, 200), Some(1_080_000));
        assert_eq!(retry_delay_ms(3, 0), None);
        let (_t, db) = db();
        let g = set(&db, "s", "goal").unwrap();
        for attempt in 1..=3 {
            let retry = arm_retry(&db, "s", &g.id, "HTTP 503").unwrap().unwrap();
            assert_eq!(retry.retry_attempts, attempt);
            assert!(!retry_ready(&db, "s", &g.id, retry.retry_after_ms.unwrap() - 1).unwrap());
            assert!(retry_ready(&db, "s", &g.id, retry.retry_after_ms.unwrap()).unwrap());
        }
        assert!(arm_retry(&db, "s", &g.id, "HTTP 503").unwrap().is_none());
        begin_input(&db, "s").unwrap();
        let resumed = active(&db, "s").unwrap().unwrap();
        assert_eq!(resumed.retry_attempts, 0);
        arm_retry(&db, "s", &resumed.id, "HTTP 503")
            .unwrap()
            .unwrap();
        clear(&db, "s").unwrap();
        assert!(!retry_ready(&db, "s", &resumed.id, i64::MAX).unwrap());
    }
}
