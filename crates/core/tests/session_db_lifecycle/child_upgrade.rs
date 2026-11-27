use super::{shard, Worker, ID};
use anyhow::Result;
use deepseek_code_core::db::session_store::{Stores, WorkerDirectory};
use deepseek_code_core::native_engine::{child_session::*, provider_manager::ResolvedProvider};
use serde_json::json;
use std::{fs::File, path::Path};

pub(super) fn run(stores: &Stores, root: &Path, contended: bool) -> Result<()> {
    let parent_id = "00000000-0000-4000-8000-000000000001";
    let parent = stores.create_code_session(root, "fixture", "fixture", parent_id)?;
    stores.create_code_session(root, "fixture", "fixture", ID)?;
    stores.save_child_owner(
        &stores.worker_pair(parent_id, ID)?,
        &ChildOwnership {
            parent_session_id: parent_id.into(),
            agent_type: "general".into(),
            description: "resume".into(),
            mode: "background".into(),
            status: "completed".into(),
            job_id: None,
            created_at: "2026-01-01".into(),
            updated_at: "2026-01-01".into(),
            last_error: None,
        },
    )?;
    let _lease = parent.acquire_execution()?;
    let guard = File::open(root.join("state/locks/session").join(ID))?;
    guard.lock_shared()?;
    let db = rusqlite::Connection::open(shard(root))?;
    db.execute_batch(
        "DROP TRIGGER first_chat_text_edit; DROP TRIGGER first_chat_typed_insert;
         DROP TRIGGER first_chat_typed_edit; DROP TRIGGER first_chat_typed_delete;
         DROP TABLE first_chat_receipts; PRAGMA user_version=11;",
    )?;
    if contended {
        let error = stores.open_session(ID).err().expect("upgrade must be busy");
        let message = error.to_string();
        assert!(
            message.contains("upgrade") && message.contains("retry"),
            "{error:#}"
        );
        let version: i32 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
        assert_eq!(version, 11);
        drop(guard);
        stores.open_session(ID)?;
    } else {
        let provider = ResolvedProvider {
            provider: serde_json::from_value(
                json!({"id":"fixture","name":"fixture","base_url":"http://127.0.0.1:1","api_key":"fixture","api_format":"openai","models":[],"enabled":true}),
            )?,
            model: serde_json::from_value(
                json!({"id":"fixture","name":"fixture","enabled":true,"supports_vision":false,"supports_web_search":false}),
            )?,
        };
        let request = ChildSpawn {
            data_dir: root.into(),
            parent_session_id: parent_id.into(),
            parent_workspace: root.into(),
            workspace: ChildWorkspace::Directory(root.into()),
            prompt: "continue".into(),
            description: "resume".into(),
            agent_type: "general".into(),
            model: None,
            effort: None,
            system_prompt_append: None,
            title: "resume".into(),
            resume_session: Some(ID.into()),
            reserved_session_id: None,
            reserved_job_id: None,
            extra_env: vec![],
            executable_override: Some(root.join("missing-supervisor")),
            invocation: None,
            member_binding: None,
            allow_workspace_move: false,
            fork: false,
        };
        let config = ChildLaunchConfig {
            provider,
            permission_mode: "ask".into(),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        drop(guard);
        let error = runtime
            .block_on(spawn_child_session(config, request))
            .unwrap_err();
        // Preparation must finish and reach the deliberately missing executable.
        assert_eq!(
            error.to_string(),
            "Starting background supervisor",
            "{error:#}"
        );
        assert_eq!(stores.child_owner(ID)?.unwrap().status, "failed");
    }
    let version: i32 = db.pragma_query_value(None, "user_version", |r| r.get(0))?;
    assert!(version > 11);
    Ok(())
}

#[test]
fn resume_upgrades_owned_child_before_holding_pair() -> Result<()> {
    let root = tempfile::tempdir()?;
    Worker::start(root.path(), "resume-old-child").expect_success();
    Ok(())
}

#[test]
fn contended_upgrade_returns_retryable_error() -> Result<()> {
    let root = tempfile::tempdir()?;
    Worker::start(root.path(), "contended-upgrade").expect_success();
    Ok(())
}
