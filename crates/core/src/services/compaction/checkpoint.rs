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
    saved: Saved,
}

impl<'a> Checkpoint<'a> {
    pub fn open(
        db: &'a DbManager,
        session: &'a str,
        provider: &ResolvedProvider,
        system: &str,
        input: &str,
    ) -> Result<Self> {
        // Hash credentials/configuration without persisting either as plaintext cache metadata.
        let fingerprint = digest(
            &json!({"version":1,"provider":provider.provider,"model":provider.model,"effort":provider.model.effort,"system":system,"input":input}),
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
        Ok(Self { db, session, saved })
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
    fn completed_summaries_require_matching_history_provider_model_and_instructions() {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(dir.path().join("data")).unwrap();
        let db = paths.open_database().unwrap();
        let provider = crate::native_engine::provider_manager::ProviderManager::in_memory(vec![serde_json::from_value(json!({"id":"p","name":"p","base_url":"http://localhost","api_key":"secret-fixture-key","api_format":"openai","enabled":true,"models":[{"id":"gpt-6-astra","name":"m","enabled":true,"max_tokens":8192,"supports_web_search":false}]})).unwrap()], dir.path().join("providers.json")).resolve_selected("p", "gpt-6-astra").unwrap();
        let request = Checkpoint::request_key("system", "fragment", 4096);
        Checkpoint::open(&db, "session", &provider, "system", "history")
            .unwrap()
            .save(request.clone(), "Completed summary".into())
            .unwrap();
        let reopened = paths.open_database().unwrap();
        assert!(
            Checkpoint::open(&reopened, "session", &provider, "system", "history")
                .unwrap()
                .get(&request)
                .is_some()
        );
        for (session, system, input) in [
            ("other", "system", "history"),
            ("session", "new system", "history"),
            ("session", "system", "changed history"),
        ] {
            assert!(
                Checkpoint::open(&reopened, session, &provider, system, input)
                    .unwrap()
                    .get(&request)
                    .is_none()
            );
        }
        let mut changed = provider.clone();
        changed.model.max_tokens = Some(2048);
        assert!(
            Checkpoint::open(&reopened, "session", &changed, "system", "history")
                .unwrap()
                .get(&request)
                .is_none()
        );
        changed = provider.clone();
        changed.provider.base_url.push_str("/different");
        assert!(
            Checkpoint::open(&reopened, "session", &changed, "system", "history")
                .unwrap()
                .get(&request)
                .is_none()
        );
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
