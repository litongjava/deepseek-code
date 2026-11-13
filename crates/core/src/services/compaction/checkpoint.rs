//! Completed summary requests are reusable; only compaction::commit installs history.
use crate::{db::DbManager, native_engine::provider_manager::ResolvedProvider};
use anyhow::Result;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;

pub(super) fn key(session: &str) -> String {
    format!("compaction.checkpoint.{session}")
}

fn digest(value: &serde_json::Value) -> String {
    ring::digest::digest(&ring::digest::SHA256, value.to_string().as_bytes())
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Serialize, Deserialize)]
struct Saved {
    fingerprint: String,
    completed: BTreeMap<String, String>,
}

pub(crate) struct Checkpoint<'a> {
    db: &'a DbManager,
    pub session: &'a str,
    pub options: crate::native_engine::request_options::RequestOptions,
    saved: Saved,
}

impl<'a> Checkpoint<'a> {
    pub fn open(
        db: &'a DbManager,
        session: &'a str,
        provider: &ResolvedProvider,
        system: &str,
    ) -> Result<Self> {
        // Hash credentials/configuration without persisting either as plaintext cache metadata.
        // The transcript is not part of this compatibility domain: it changes
        // whenever a cancelled compaction is followed by new messages, and each
        // completed request is already identified by its own exact request key.
        // Version 3 separates that request key from the configuration domain so
        // unchanged fragments survive a later turn; earlier layouts are dropped.
        let fingerprint = digest(
            &json!({"version":3,"provider":provider.provider,"model":provider.model,"effort":provider.model.effort,"system":system}),
        );
        let value: Option<String> = db.with_conn(|conn| {
            conn.query_row(
                "SELECT value FROM peripheral_settings WHERE key=?1",
                [key(session)],
                |r| r.get(0),
            )
            .optional()
        })??;
        let saved = value
            .and_then(|v| serde_json::from_str::<Saved>(&v).ok())
            .filter(|v| v.fingerprint == fingerprint)
            .unwrap_or_else(|| Saved {
                fingerprint,
                completed: BTreeMap::new(),
            });
        Ok(Self {
            db,
            session,
            saved,
            options: crate::native_engine::request_options::RequestOptions::from_db(
                db,
                Some(session),
                "compaction",
            ),
        })
    }

    pub fn request_key(system: &str, input: &str, output: u32) -> String {
        digest(&json!({"system":system,"input":input,"output":output}))
    }

    pub fn get(&self, request: &str) -> Option<String> {
        self.saved
            .completed
            .get(request)
            .filter(|s| !s.trim().is_empty() && s.chars().count() <= 32_000)
            .cloned()
    }

    pub fn save(&mut self, request: String, summary: String) -> Result<()> {
        self.saved.completed.insert(request, summary);
        let value = serde_json::to_string(&self.saved)?;
        self.db.with_conn(|conn| conn.execute("INSERT INTO peripheral_settings(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [key(self.session), value]))??;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{RuntimePaths, SessionStore};

    #[test]
    fn completed_summaries_survive_history_growth_but_require_matching_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(dir.path().join("data")).unwrap();
        let db = paths.open_database().unwrap();
        let provider = crate::native_engine::provider_manager::ProviderManager::in_memory(vec![serde_json::from_value(json!({"id":"p","name":"p","base_url":"http://localhost","api_key":"secret-fixture-key","api_format":"openai","enabled":true,"models":[{"id":"gpt-6-astra","name":"m","enabled":true,"max_tokens":8192,"supports_web_search":false}]})).unwrap()], dir.path().join("providers.json")).resolve_selected("p", "gpt-6-astra").unwrap();
        let request = Checkpoint::request_key("system", "fragment", 4096);
        Checkpoint::open(&db, "session", &provider, "system")
            .unwrap()
            .save(request.clone(), "Completed summary".into())
            .unwrap();
        let reopened = paths.open_database().unwrap();
        // A later turn changes the transcript, but the completed request is
        // still valid for the exact fragment it summarized.
        let cache = Checkpoint::open(&reopened, "session", &provider, "system").unwrap();
        assert_eq!(cache.get(&request).as_deref(), Some("Completed summary"));
        assert!(cache
            .get(&Checkpoint::request_key("system", "changed fragment", 4096))
            .is_none());
        // Session and instructions remain part of the compatibility domain.
        for (session, system) in [("other", "system"), ("session", "new system")] {
            assert!(Checkpoint::open(&reopened, session, &provider, system)
                .unwrap()
                .get(&request)
                .is_none());
        }
        let mut changed = provider.clone();
        changed.model.max_tokens = Some(2048);
        assert!(Checkpoint::open(&reopened, "session", &changed, "system")
            .unwrap()
            .get(&request)
            .is_none());
        changed = provider.clone();
        changed.provider.base_url.push_str("/different");
        assert!(Checkpoint::open(&reopened, "session", &changed, "system")
            .unwrap()
            .get(&request)
            .is_none());
        let value: String = db
            .with_conn(|c| {
                c.query_row(
                    "SELECT value FROM peripheral_settings WHERE key=?1",
                    [key("session")],
                    |r| r.get(0),
                )
            })
            .unwrap()
            .unwrap();
        assert!(!value.contains("secret-fixture-key"));
    }

    #[test]
    fn a_checkpoint_from_the_previous_planning_strategy_is_not_reused() {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(dir.path().join("data")).unwrap();
        let db = paths.open_database().unwrap();
        let provider = crate::native_engine::provider_manager::ProviderManager::in_memory(vec![serde_json::from_value(json!({"id":"p","name":"p","base_url":"http://localhost","api_key":"secret-fixture-key","api_format":"openai","enabled":true,"models":[{"id":"gpt-6-astra","name":"m","enabled":true,"max_tokens":8192,"supports_web_search":false}]})).unwrap()], dir.path().join("providers.json")).resolve_selected("p", "gpt-6-astra").unwrap();
        let request = Checkpoint::request_key("system", "fragment", 4096);
        // A version 1 fingerprint is what the earlier strategy stored.
        let fingerprint = digest(
            &json!({"version":1,"provider":provider.provider,"model":provider.model,"effort":provider.model.effort,"system":"system","input":"history"}),
        );
        let stale =
            json!({"fingerprint":fingerprint,"completed":{ request.clone(): "Stale summary" }});
        db.with_conn(|conn| {
            conn.execute(
                "INSERT INTO peripheral_settings(key,value) VALUES(?1,?2)",
                [key("session"), stale.to_string()],
            )
        })
        .unwrap()
        .unwrap();
        assert!(
            Checkpoint::open(&db, "session", &provider, "system")
                .unwrap()
                .get(&request)
                .is_none(),
            "a previous strategy's fragment must not be reused"
        );
    }

    #[test]
    fn request_keys_separate_the_fixed_target_and_the_output_budget() {
        let key = Checkpoint::request_key("system", "fragment at 2048 tokens", 4096);
        // The same fixed target sent with a raised budget is a different request.
        assert_ne!(
            key,
            Checkpoint::request_key("system", "fragment at 2048 tokens", 8192)
        );
        // A different target is a different request at the same budget.
        assert_ne!(
            key,
            Checkpoint::request_key("system", "fragment at 4096 tokens", 4096)
        );
        assert_ne!(
            key,
            Checkpoint::request_key("other system", "fragment at 2048 tokens", 4096)
        );
        assert_eq!(
            key,
            Checkpoint::request_key("system", "fragment at 2048 tokens", 4096)
        );
    }

    #[test]
    fn empty_saved_summaries_are_never_returned() {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(dir.path().join("data")).unwrap();
        let db = paths.open_database().unwrap();
        let provider = crate::native_engine::provider_manager::ProviderManager::in_memory(vec![serde_json::from_value(json!({"id":"p","name":"p","base_url":"http://localhost","api_key":"fixture","api_format":"openai","enabled":true,"models":[{"id":"gpt-6-astra","name":"m","enabled":true,"max_tokens":8192,"supports_web_search":false}]})).unwrap()], dir.path().join("providers.json")).resolve_selected("p", "gpt-6-astra").unwrap();
        let request = Checkpoint::request_key("system", "fragment", 4096);
        Checkpoint::open(&db, "session", &provider, "system")
            .unwrap()
            .save(request.clone(), "   \n".into())
            .unwrap();
        assert!(Checkpoint::open(&db, "session", &provider, "system")
            .unwrap()
            .get(&request)
            .is_none());
    }

    #[test]
    fn final_boundary_clears_checkpoint_in_the_same_transaction() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::new(
            RuntimePaths::new(dir.path().join("data"))
                .unwrap()
                .open_database()
                .unwrap(),
        );
        let id = store.create(dir.path(), "model", "provider").unwrap();
        store.append(&id, "user", "Task").unwrap();
        store.append(&id, "assistant", "Progress").unwrap();
        store
            .db
            .with_conn(|c| {
                c.execute(
                    "INSERT INTO peripheral_settings(key,value) VALUES(?1,'{}')",
                    [key(&id)],
                )
            })
            .unwrap()
            .unwrap();
        let prepared = super::super::prepare(&store.db, &id).unwrap();
        super::super::commit(&store.db, prepared, "Final summary").unwrap();
        let remaining: i64 = store
            .db
            .with_conn(|c| {
                c.query_row(
                    "SELECT COUNT(*) FROM peripheral_settings WHERE key=?1",
                    [key(&id)],
                    |r| r.get(0),
                )
            })
            .unwrap()
            .unwrap();
        assert_eq!(remaining, 0);
        assert_eq!(store.history(&id).unwrap().len(), 3);
    }
}
