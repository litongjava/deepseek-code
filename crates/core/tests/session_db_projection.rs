use anyhow::{bail, Result};
use deepseek_code_core::db::{session_store::*, turn_repo::TurnStatus};
use deepseek_code_core::permissions::PermissionMode;
use deepseek_code_core::services::{compaction::GeneratedSummary, goal_proposals::ProposalState};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::{
    fs,
    time::{Duration, Instant},
};
use tempfile::tempdir;

const A: &str = "10000000-1234-4234-9234-123456789abc";

#[test]
fn search_indexes_rollback_with_content_and_cursors_bind_to_the_incarnation() -> Result<()> {
    let root = tempdir()?;
    let stores = Stores::open(root.path())?;
    let a = stores.create(A, Default::default())?;
    let b = stores.create("b", Default::default())?;
    let big = format!("{}needle old", "x".repeat(130_000));
    let before = a.write(|w| w.append("user", &big))?;
    let boundary = a.approve_plan("needle hidden boundary")?;
    let after = a.write(|w| w.append("assistant", "needle new"))?;
    let search = |s: &SessionDb, q: Value| -> Result<Value> {
        Ok(serde_json::from_str(&s.context_search(q)?)?)
    };
    let q = json!({"query":"needle","limit":1});
    let page = search(&a, q.clone())?;
    assert_eq!(page["matches"][0]["message_id"], after);
    let cursor = page["next_cursor"].as_str().unwrap();
    let next = json!({"query":"needle","limit":1,"cursor":cursor});
    assert_eq!(
        search(&a, next.clone())?["matches"][0]["message_id"],
        before
    );
    assert!(b.context_search(next.clone()).is_err());
    assert!(a
        .context_search(json!({"query":"other","cursor":cursor}))
        .is_err());
    let page = search(
        &a,
        json!({"query":"nee.le","regex":true,"range":"before","boundary_id":boundary}),
    )?;
    assert_eq!(page["matches"].as_array().unwrap().len(), 1);
    assert_eq!(page["matches"][0]["message_id"], before);
    assert_eq!(
        search(&a, json!({"query":"needle","range":"after"}))?["matches"][0]["message_id"],
        after
    );
    let db = Connection::open(root.path().join("data/sessions").join(A).join("session.db"))?;
    let chunks = || {
        db.query_row(
            "SELECT count(*) FROM context_search_chunks WHERE id=?1",
            [&before],
            |r| r.get::<_, usize>(0),
        )
    };
    let n = chunks()?;
    assert!(n > 1);
    let aborted: Result<()> = a.write(|w| {
        w.message_content(&before, "replacement")?;
        bail!("abort after mutation")
    });
    assert!(aborted.is_err());
    assert_eq!(chunks()?, n);
    assert_eq!(
        search(&a, json!({"query":"needle old"}))?["matches"][0]["message_id"],
        before
    );
    a.write(|w| w.message_content(&before, "replacement"))?;
    assert_eq!(chunks()?, 0);
    assert!(search(&a, json!({"query":"needle old"}))?["matches"]
        .as_array()
        .unwrap()
        .is_empty());
    drop(db);
    a.delete()?;
    assert!(stores
        .create(A, Default::default())?
        .context_search(next)
        .is_err());
    Ok(())
}

#[test]
fn projection_replaces_usage_and_recomputes_after_fork_rewind_and_restart() -> Result<()> {
    let root = tempdir()?;
    {
        let s = Stores::open(root.path())?;
        let a = s.create(A, Default::default())?;
        let (first, assistant) = a.write(|w| {
            let first = w.append("user", "first")?;
            let turn = w.start_turn()?;
            let m = w.segment(&turn)?;
            w.snapshots(
                &m,
                &json!([{"id":"old","totalTokens":100},{"id":"kept","inputTokens":10}]),
            )?;
            w.set_request_metadata("kept", &json!({"traced":true}))?;
            w.set_message_metadata(&m, &json!({"type":"trace"}))?;
            w.finish_turn(&turn, TurnStatus::Completed, 1)?;
            w.append("user", "latest")?;
            w.set_git_branch(Some("feature"))?;
            Ok((first, m))
        })?;
        a.write(|w| {
            w.snapshots(
                &assistant,
                &json!([{"id":"kept","inputTokens":4,"outputTokens":3}]),
            )
        })?;
        let p = s.global().summary(A)?.unwrap();
        assert_eq!(
            (
                p.request_count,
                p.total_tokens,
                p.input_tokens,
                p.output_tokens
            ),
            (1, 7, 4, 3)
        );
        assert_eq!(p.first_prompt.as_deref(), Some("first"));
        assert_eq!(p.latest_prompt.as_deref(), Some("latest"));
        assert_eq!(p.git_branch.as_deref(), Some("feature"));
        let fork = a.branch("fork", Branch::Full)?;
        let child = s.global().summary("fork")?.unwrap();
        assert_eq!(
            (child.count, child.total_tokens, child.request_count),
            (3, 7, 1)
        );
        fork.read(|r| {
            let m = &r.messages()?[1].id;
            let requests = r.requests(m)?;
            assert_ne!(requests[0]["id"], "kept");
            assert_eq!(
                r.request_metadata(requests[0]["id"].as_str().unwrap())?,
                Some(json!({"traced":true}))
            );
            assert_eq!(r.message_metadata(m)?, Some(json!({"type":"trace"})));
            Ok(())
        })?;
        a.write(|w| w.rewind_from(&assistant))?;
        assert_eq!(a.read(|r| r.messages())?[0].id, first);
    }
    let s = Stores::open(root.path())?;
    let p = s.global().summary(A)?.unwrap();
    assert_eq!(
        (
            p.count,
            p.latest_prompt.as_deref(),
            p.status,
            p.total_tokens,
            p.request_count
        ),
        (1, Some("first"), None, 0, 0)
    );
    Ok(())
}

#[test]
fn lost_projection_repairs_on_open_startup_and_unattended_rotation() -> Result<()> {
    let root = tempdir()?;
    let gpath = root.path().join("global.db");
    {
        let s = Stores::open(root.path())?;
        let a = s.create(A, Default::default())?;
        let g = Connection::open(&gpath)?;
        g.execute_batch("BEGIN IMMEDIATE")?;
        a.write(|w| w.append("user", "open repair"))?;
        assert_eq!(s.global().summary(A)?.unwrap().count, 0);
        g.execute_batch("COMMIT")?;
        s.open_session(A)?;
        assert_eq!(s.global().summary(A)?.unwrap().count, 1);
        g.execute_batch("BEGIN IMMEDIATE")?;
        a.write(|w| w.append("user", "startup repair"))?;
        g.execute_batch("COMMIT")?;
    }
    let s = Stores::open(root.path())?;
    assert_eq!(s.global().summary(A)?.unwrap().count, 2);
    let handles = (0..19)
        .map(|i| s.create(&format!("idle-{i}"), Default::default()))
        .collect::<Result<Vec<_>>>()?;
    let g = Connection::open(&gpath)?;
    g.execute_batch("BEGIN IMMEDIATE")?;
    for h in &handles {
        h.write(|w| w.append("user", "unattended"))?;
    }
    drop(handles);
    assert_eq!(
        g.query_row(
            "SELECT count(*) FROM conversations WHERE message_count=0",
            [],
            |r| r.get::<_, usize>(0)
        )?,
        19
    );
    g.execute_batch("COMMIT")?;
    let deadline = Instant::now() + Duration::from_secs(50);
    loop {
        let stale: usize = g.query_row(
            "SELECT count(*) FROM conversations WHERE message_count=0",
            [],
            |r| r.get(0),
        )?;
        if stale == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{stale} idle projections still stale"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

#[test]
fn interrupted_compaction_resumes_checkpoint_and_commits_one_boundary() -> Result<()> {
    let root = tempdir()?;
    let provider = deepseek_code_core::native_engine::provider_manager::ProviderManager::in_memory(
        vec![serde_json::from_value(json!({"id":"p","name":"p","base_url":"http://localhost","api_key":"checkpoint-fixture",
        "api_format":"openai","enabled":true,"models":[{"id":"m","name":"m","enabled":true,"max_tokens":8192,"supports_web_search":false}]}))?],
        root.path().join("config/providers.json")).resolve_selected("p","m").unwrap();
    let key = CheckpointKey::new(&provider, "configuration", "system", "fragment", 1024);
    let other = CheckpointKey::new(&provider, "configuration", "system", "changed", 1024);
    {
        let s = Stores::open(root.path())?;
        let a = s.create(A, Default::default())?;
        a.write(|w| {
            w.append("user", "task")?;
            w.append("assistant", "answer")?;
            w.request_compaction("retain", 50, 100)
        })?;
        let owner = a.delivery_owner()?;
        let claim = owner.claim_compaction()?.unwrap();
        assert_eq!(claim.keep, "retain");
        a.write(|w| w.save_compaction_checkpoint(&key, "partial result"))?;
        a.write(|w| w.append("user", "new turn"))?;
        assert!(owner.finish_compaction(&claim, &"stale".into()).is_err());
    }
    let s = Stores::open(root.path())?;
    let a = s.open_session(A)?;
    let owner = a.delivery_owner()?;
    let claim = owner.claim_compaction()?.unwrap();
    assert!(claim.transcript.contains("new turn"));
    assert_eq!(
        a.read(|r| r.compaction_checkpoint(&key))?.as_deref(),
        Some("partial result")
    );
    assert!(a.read(|r| r.compaction_checkpoint(&other))?.is_none());
    let native = json!({"compaction":true,"opaque":"retained"});
    let id = owner.finish_compaction(
        &claim,
        &GeneratedSummary {
            text: "done".into(),
            native: Some(native.clone()),
        },
    )?;
    assert!(owner.claim_compaction()?.is_none());
    assert!(owner
        .finish_compaction(&claim, &"duplicate".into())
        .is_err());
    drop(owner);
    assert_eq!(a.read(|r| r.compaction_count())?, 1);
    assert!(a.read(|r| r.compaction_checkpoint(&key))?.is_none());
    assert_eq!(a.read(|r| r.native_compaction_boundary(&id))?, Some(native));
    assert_eq!(
        a.branch("branch", Branch::Full)?
            .read(|r| r.compaction_count())?,
        0
    );
    Ok(())
}

#[test]
fn typed_state_and_file_read_evidence_survive_resume_with_session_isolation() -> Result<()> {
    let root = tempdir()?;
    let file = root.path().join("input.txt");
    fs::write(&file, "before")?;
    {
        let s = Stores::open(root.path())?;
        let a = s.create(A, Default::default())?;
        assert!(a.check_file_write(&file).is_err());
        assert_eq!(
            a.read_tracked_file(&root.path().join("./input.txt"))?,
            b"before"
        );
        s.global()
            .set_setting(GlobalSetting::MachinePreference("theme"), &"dark")?;
        let proposals = serde_json::from_value::<ProposalState>(
            json!({"frontend":null,"pending":null,"declined":["old"]}),
        )?;
        a.write(|w| {
            w.set_goal("finish work")?;
            w.save_goal_proposals(&ProposalState::default(), &proposals)?;
            w.set_agent_mode(PermissionMode::PlanMode)
        })?;
        a.approve_plan("approved work")?;
        let b = s.create("other", Default::default())?;
        assert!(b.check_file_write(&file).is_err());
        assert!(b.read(|r| r.goal())?.is_none());
    }
    let s = Stores::open(root.path())?;
    let a = s.open_session(A)?;
    assert_eq!(
        s.global()
            .setting::<String>(GlobalSetting::MachinePreference("theme"))?
            .as_deref(),
        Some("dark")
    );
    assert_eq!(a.read(|r| r.goal())?.unwrap().condition, "finish work");
    assert_eq!(
        serde_json::to_value(a.read(|r| r.goal_proposals())?)?["declined"],
        json!(["old"])
    );
    assert_eq!(a.read(|r| r.agent_mode())?, Some(PermissionMode::PlanMode));
    assert!(a.read(|r| r.approved_plan_boundary())?.is_some());
    let boundary = a.approve_plan("revised approval")?;
    assert!(a
        .write(|w| w.save_goal_proposals(&ProposalState::default(), &ProposalState::default()))
        .is_err());
    assert!(a.write(|w| w.clear_goal())?);
    assert!(a.read(|r| r.goal())?.is_none());
    a.check_file_write(&file)?;
    fs::write(&file, "changed since read")?;
    assert!(a
        .check_file_write(&file)
        .unwrap_err()
        .to_string()
        .contains("modified since read"));
    a.read_tracked_file(&file)?;
    a.check_file_write(&file)?;
    assert!(a.check_file_write(&root.path().join("new.txt")).is_ok());
    let branched = a.branch("clear", Branch::Clear)?;
    assert!(branched.check_file_write(&file).is_err());
    a.write(|w| w.rewind_from(&boundary))?;
    assert!(a.read(|r| r.approved_plan_boundary())?.is_none());
    Ok(())
}

fn remove_v4_features(c: &Connection) -> Result<()> {
    c.execute_batch("DROP INDEX idx_execution_turns_conversation")?;
    for name in ["thinking", "tool", "trajectory"] {
        for action in ["insert", "update", "delete"] {
            c.execute_batch(&format!("DROP TRIGGER transcript_{name}_{action}"))?;
        }
    }
    c.execute_batch(
        "DROP TRIGGER cancel_rewound_compaction; DROP TABLE compaction_cancellations;
        ALTER TABLE compaction_request DROP COLUMN target_message;
        ALTER TABLE file_read_stamps DROP COLUMN digest;
        DROP INDEX projection_prompt_order",
    )?;
    for name in ["message", "typed", "request"] {
        for action in ["insert", "update", "delete"] {
            c.execute_batch(&format!("DROP TRIGGER projection_{name}_{action}"))?;
        }
    }
    c.execute_batch("DROP TABLE projection_state; DROP TABLE request_metric_deltas")?;
    Ok(())
}

#[test]
fn upgrading_a_populated_p2_root_backfills_chunks_and_usage() -> Result<()> {
    let root = tempdir()?;
    let text = format!("{}archived needle", "x".repeat(70_000));
    let seal = {
        let s = Stores::open(root.path())?;
        let a = s.create(A, Default::default())?;
        a.write(|w| {
            let m = w.append("assistant", &text)?;
            w.snapshots(&m, &json!([{"id":"old","totalTokens":19}]))
        })?;
        a.incarnation().to_owned()
    };
    let path = root.path().join("data/sessions").join(A).join("session.db");
    {
        let c = Connection::open(&path)?;
        c.execute_batch("BEGIN IMMEDIATE")?;
        for owner in ["messages", "tool_calls"] {
            for operation in ["insert", "update", "delete"] {
                c.execute_batch(&format!("DROP TRIGGER context_search_{owner}_{operation}"))?;
            }
        }
        remove_v4_features(&c)?;
        for table in [
            "context_search_chunks",
            "context_search_sources",
            "message_metadata",
            "request_metadata",
            "approved_plan_boundaries",
            "file_read_stamps",
            "peripheral_settings",
            "compaction_checkpoint",
            "compaction_request",
        ] {
            c.execute_batch(&format!("DROP TABLE {table}"))?;
        }
        c.execute_batch("PRAGMA user_version=2; COMMIT")?;
        let g = Connection::open(root.path().join("global.db"))?;
        g.execute_batch(
            "BEGIN IMMEDIATE; DROP TABLE global_settings;
            DROP INDEX inbox_notices;
            ALTER TABLE session_inbox DROP COLUMN notified;
            ALTER TABLE session_inbox DROP COLUMN recipient_source;
            ALTER TABLE conversations DROP COLUMN request_count;
            ALTER TABLE conversations DROP COLUMN input_tokens;
            ALTER TABLE conversations DROP COLUMN output_tokens;
            ALTER TABLE conversations DROP COLUMN total_tokens;
            PRAGMA user_version=3; COMMIT",
        )?;
    }
    let s = Stores::open(root.path())?;
    let p = s.global().summary(A)?.unwrap();
    assert_eq!((p.count, p.request_count, p.total_tokens), (1, 1, 19));
    let a = s.open_session(A)?;
    assert_eq!(a.incarnation(), seal);
    let result: Value =
        serde_json::from_str(&a.context_search(json!({"query":"archived needle"}))?)?;
    assert_eq!(result["matches"].as_array().unwrap().len(), 1);
    // The reader selects its bounded query plan with INDEXED BY. Losing a
    // required index makes this shard unreadable even if the tables survive.
    Connection::open(&path)?.execute_batch("DROP INDEX context_search_boundary_order")?;
    assert!(s
        .open_session(A)
        .err()
        .unwrap()
        .to_string()
        .contains("search index"));
    Ok(())
}

#[test]
fn tool_search_fragments_follow_result_replacement_and_rollback() -> Result<()> {
    use deepseek_code_core::db::message_repo::ToolCallRow;
    let root = tempdir()?;
    let a = Stores::open(root.path())?.create(A, Default::default())?;
    let m = a.write(|w| w.append("assistant", "reply"))?;
    let write = |w: &SessionWrite<'_>, name: &str, id: &str, text: &str| {
        w.tool(
            &ToolCallRow {
                id: id.into(),
                message_id: m.clone(),
                name: name.into(),
                input: None,
                output: Some(text.into()),
                is_error: false,
                sort_order: 0,
            },
            None,
        )
    };
    let large = format!("{} unique-result", "z".repeat(100_000));
    a.write(|w| {
        write(w, "custom", "original", &large)?;
        write(w, "ContextSearch", "excluded", &large)
    })?;
    let search = || -> Result<Value> {
        Ok(serde_json::from_str(&a.context_search(
            json!({"query":"unique-result","role":"tool"}),
        )?)?)
    };
    let failed: Result<()> = a.write(|w| {
        write(w, "custom", "original", "changed")?;
        bail!("rollback tool replacement")
    });
    assert!(failed.is_err());
    let hits = search()?;
    assert_eq!(hits["matches"].as_array().unwrap().len(), 1);
    assert_eq!(hits["matches"][0]["tool_call_id"], "original");
    a.write(|w| write(w, "custom", "original", "changed"))?;
    assert!(search()?["matches"].as_array().unwrap().is_empty());
    Ok(())
}

#[test]
fn clamped_summary_preserves_committed_history_and_reverses_on_replacement() -> Result<()> {
    let root = tempdir()?;
    let s = Stores::open(root.path())?;
    let a = s.create(A, Default::default())?;
    let m = a.write(|w| {
        let m = w.append("assistant", "saved")?;
        w.snapshots(
            &m,
            &json!([{"id":"one","inputTokens":i64::MAX},{"id":"two","inputTokens":1}]),
        )?;
        Ok(m)
    })?;
    assert_eq!(a.read(|r| r.messages())?[0].content, "saved");
    assert_eq!(s.global().summary(A)?.unwrap().input_tokens, i64::MAX);
    a.write(|w| w.snapshots(&m, &json!([{"id":"two","inputTokens":1}])))?;
    let p = s.global().summary(A)?.unwrap();
    assert_eq!((p.count, p.input_tokens), (1, 1));
    Ok(())
}

#[test]
fn replacing_anonymous_snapshot_metrics_preserves_independent_requests() -> Result<()> {
    let root = tempdir()?;
    let stores = Stores::open(root.path())?;
    let a = stores.create(A, Default::default())?;
    let message = a.write(|w| {
        let m = w.append("assistant", "result")?;
        w.normalized_request(&m, &json!({"inputTokens": 11}))?;
        w.snapshots(&m, &json!([{"inputTokens": 3}, {"inputTokens": 3}]))?;
        w.normalized_request(&m, &json!({"inputTokens": 7}))?;
        Ok(m)
    })?;
    let fork = a.branch("anonymous-fork", Branch::Full)?;
    let copied = fork.read(|r| Ok(r.messages()?[0].id.clone()))?;
    for (session, m) in [(&a, &message), (&fork, &copied)] {
        let before = stores.global().summary(session.id())?.unwrap();
        assert_eq!((before.request_count, before.input_tokens), (4, 24));
        session.write(|w| w.snapshots(m, &json!([{"inputTokens": 2}])))?;
        let after = stores.global().summary(session.id())?.unwrap();
        assert_eq!((after.request_count, after.input_tokens), (3, 20));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn syscall_read_bytes(path: &str) -> Result<u64> {
    let io = std::fs::read_to_string(path)?;
    Ok(io
        .lines()
        .find_map(|l| l.strip_prefix("rchar: "))
        .unwrap()
        .parse()?)
}

#[cfg(target_os = "linux")]
fn large_private_sidecar(stores: &Stores) -> Result<SessionDb> {
    let a = stores.create(
        A,
        CreateOptions {
            first_message: Some(FirstMessage {
                origin: "typed".into(),
                text: "private".into(),
                typed: Some(json!([{"type":"image","data":"a".repeat(8 << 20)}])),
            }),
            ..Default::default()
        },
    )?;
    a.approve_plan("boundary")?;
    a.write(|w| {
        w.append("assistant", "public needle")
    })?;
    Ok(a)
}

#[cfg(target_os = "linux")]
#[test]
fn fix_search_checkout_obeys_the_filtered_read_budget() -> Result<()> {
    let root = tempdir()?;
    let stores = Stores::open(root.path())?;
    let a = large_private_sidecar(&stores)?;
    let start = syscall_read_bytes("/proc/thread-self/io")?;
    let page: Value = serde_json::from_str(&a.context_search(
        json!({"query":"needle","role":"assistant","range":"after","regex":true}),
    )?)?;
    let bytes = syscall_read_bytes("/proc/thread-self/io")? - start;
    assert_eq!(page["matches"].as_array().unwrap().len(), 1);
    eprintln!("filtered checkout and search read {bytes} bytes");
    assert!(
        bytes < 4 << 20,
        "read {bytes} bytes outside the selected range"
    );
    drop(a);
    drop(stores);
    let reopened = Stores::open(root.path())?;
    let start = syscall_read_bytes("/proc/thread-self/io")?;
    reopened.open_session(A)?;
    let bytes = syscall_read_bytes("/proc/thread-self/io")? - start;
    assert!(
        bytes < 4 << 20,
        "reopening the resolver rescanned {bytes} bytes"
    );
    // A valid external repair invalidates admission once. Further checkouts of
    // that same edited file must still obey the query budget.
    let db = Connection::open(root.path().join("data/sessions").join(A).join("session.db"))?;
    db.execute(
        "UPDATE message_user_content SET content_json=content_json",
        [],
    )?;
    let verified = reopened.open_session(A)?;
    let start = syscall_read_bytes("/proc/thread-self/io")?;
    verified.read(|_| Ok(()))?;
    assert!(syscall_read_bytes("/proc/thread-self/io")? - start < 4 << 20);
    db.execute(
        "UPDATE message_user_content SET content_json=cast(content_json AS BLOB)",
        [],
    )?;
    assert!(verified.read(|_| Ok(())).is_err());
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn fix_idle_rotation_reads_only_bounded_summary_state() -> Result<()> {
    const NAME: &str = "fix_idle_rotation_reads_only_bounded_summary_state";
    if std::env::var_os("I46_IDLE_PROJECTION_CHILD").is_none() {
        let status = std::process::Command::new(std::env::current_exe()?)
            .args(["--exact", NAME, "--nocapture"])
            .env("I46_IDLE_PROJECTION_CHILD", "1")
            .status()?;
        assert!(status.success());
        return Ok(());
    }
    let root = tempdir()?;
    let stores = Stores::open(root.path())?;
    let a = large_private_sidecar(&stores)?;
    let g = Connection::open(root.path().join("global.db"))?;
    g.execute_batch("BEGIN IMMEDIATE")?;
    a.write(|w| w.append("user", "idle repair"))?;
    g.execute_batch("COMMIT")?;
    drop(a);
    let before = syscall_read_bytes("/proc/self/io")?;
    std::thread::sleep(std::time::Duration::from_millis(4700));
    let bytes = syscall_read_bytes("/proc/self/io")? - before;
    assert_eq!(stores.global().summary(A)?.unwrap().count, 4);
    eprintln!("stale and current repair passes read {bytes} bytes");
    assert!(bytes < 4 << 20, "maintenance read {bytes} bytes");
    Ok(())
}

#[test]
fn fix_overflowing_usage_publishes_and_forks() -> Result<()> {
    let root = tempdir()?;
    let s = Stores::open(root.path())?;
    let a = s.create(A, Default::default())?;
    let m = a.write(|w| {
        let m = w.append("assistant","response")?;
        w.snapshots(&m,&json!([{"id":"big","inputTokens":i64::MAX,"outputTokens":10},{"id":"small","inputTokens":7}]))?;
        Ok(m)
    })?;
    let fork = a.branch("large-usage", Branch::Full)?;
    for id in [A, fork.id()] {
        let p = s.global().summary(id)?.unwrap();
        assert_eq!(
            (
                p.request_count,
                p.input_tokens,
                p.output_tokens,
                p.total_tokens
            ),
            (2, i64::MAX, 10, i64::MAX)
        );
    }
    a.write(|w| w.snapshots(&m, &json!([{"id":"small","inputTokens":7}])))?;
    assert_eq!(s.global().summary(A)?.unwrap().total_tokens, 7);
    Ok(())
}

#[test]
fn fix_plan_origin_survives_reentry_and_resume() -> Result<()> {
    let root = tempdir()?;
    let s = Stores::open(root.path())?;
    for (id, origin, want) in [
        (A, PermissionMode::BypassPermissions, "true"),
        (
            "20000000-1234-4234-9234-123456789abc",
            PermissionMode::AskPermissions,
            "false",
        ),
    ] {
        let a = s.create(id, Default::default())?;
        a.write(|w| {
            w.set_agent_mode(origin)?;
            w.set_agent_mode(PermissionMode::PlanMode)
        })?;
        s.open_session(id)?
            .write(|w| w.set_agent_mode(PermissionMode::PlanMode))?;
        let c = Connection::open(
            root.path()
                .join("data/sessions")
                .join(id)
                .join("session.db"),
        )?;
        let value: String = c.query_row(
            "SELECT value FROM peripheral_settings WHERE key=?1",
            [format!("plan.return-bypass.{id}")],
            |r| r.get(0),
        )?;
        assert_eq!(value, want);
        assert_eq!(a.read(|r| r.plan_returns_to_bypass(true))?, want == "true");
        assert!(!a.read(|r| r.plan_returns_to_bypass(false))?);
        a.write(|w| {
            w.set_agent_mode(PermissionMode::AskPermissions)?;
            w.set_agent_mode_from(
                PermissionMode::PlanMode,
                PermissionMode::BypassPermissions,
                false,
            )
        })?;
        assert!(!a.read(|r| r.plan_returns_to_bypass(true))?);
        a.write(|w| {
            w.set_agent_mode(PermissionMode::AskPermissions)?;
            w.set_agent_mode_from(
                PermissionMode::PlanMode,
                PermissionMode::BypassPermissions,
                true,
            )
        })?;
        assert!(a.read(|r| r.plan_returns_to_bypass(true))?);
    }
    Ok(())
}

#[test]
fn fix_same_size_and_timestamp_restore_requires_another_read() -> Result<()> {
    let root = tempdir()?;
    let s = Stores::open(root.path())?;
    let path = root.path().join("tracked");
    std::fs::write(&path, b"first")?;
    let modified = path.metadata()?.modified()?;
    assert_eq!(
        s.create(A, Default::default())?.read_tracked_file(&path)?,
        b"first"
    );
    let a = s.open_session(A)?;
    std::fs::write(&path, b"later")?;
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)?
        .set_modified(modified)?;
    assert!(a.check_file_write(&path).is_err());
    assert_eq!(a.read_tracked_file(&path)?, b"later");
    a.check_file_write(&path)
}

#[test]
fn fix_rewound_compaction_target_stays_cancelled() -> Result<()> {
    let root = tempdir()?;
    let s = Stores::open(root.path())?;
    let a = s.create(A, Default::default())?;
    let tail = a.write(|w| {
        w.append("user", "first")?;
        let target = w.append("assistant", "target")?;
        w.request_compaction("old instructions", 50, 100)?;
        Ok(target)
    })?;
    let owner = a.delivery_owner()?;
    let stale = owner.claim_compaction()?.unwrap();
    a.write(|w| w.rewind_from(&tail))?;
    assert!(owner
        .finish_compaction(&stale, &"stale summary".into())
        .is_err());
    drop(owner);
    assert_eq!(
        a.read(|r| r.compaction_cancellation(stale.request_id()))?
            .as_deref(),
        Some("target removed by rewind")
    );
    assert!(s
        .open_session(A)?
        .delivery_owner()?
        .claim_compaction()?
        .is_none());
    a.write(|w| w.request_compaction("ineligible", 50, 100))?;
    assert!(a.delivery_owner()?.claim_compaction()?.is_none());
    a.write(|w| w.append("assistant", "unrelated"))?;
    assert!(a.delivery_owner()?.claim_compaction()?.is_none());
    a.write(|w| w.request_compaction("replacement", 50, 100))?;
    let owner = a.delivery_owner()?;
    assert!(owner.finish_compaction(&stale, &"old".into()).is_err());
    assert_eq!(owner.claim_compaction()?.unwrap().keep, "replacement");
    Ok(())
}

#[test]
fn fix_global_decoder_releases_checkout_before_entering_session() -> Result<()> {
    use serde::{Deserialize, Deserializer};
    use std::sync::{Arc, Barrier};
    thread_local! { static CONTEXT: std::cell::RefCell<Option<(SessionDb, Arc<Barrier>)>> = const {std::cell::RefCell::new(None)}; }
    struct Decoder;
    impl<'de> Deserialize<'de> for Decoder {
        fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
            let _ = String::deserialize(d)?;
            CONTEXT.with_borrow(|ctx| {
                let (a, barrier) = ctx.as_ref().unwrap();
                barrier.wait();
                a.read(|r| r.git_branch())
                    .map_err(serde::de::Error::custom)?;
                Ok(Self)
            })
        }
    }
    let root = tempdir()?;
    let s = Stores::open(root.path())?;
    let a = s.create(A, Default::default())?;
    s.global()
        .set_setting(GlobalSetting::MachinePreference("test"), &"value")?;
    let nested = s
        .global()
        .with_read_for_test(|| a.read(|r| r.git_branch()))?;
    assert!(nested.unwrap_err().to_string().contains("global checkout"));
    let barrier = Arc::new(Barrier::new(4));
    std::thread::scope(|scope| {
        let workers = (0..4)
            .map(|_| {
                let context = (a.clone(), barrier.clone());
                let g = s.global();
                scope.spawn(move || {
                    CONTEXT.set(Some(context));
                    g.setting::<Decoder>(GlobalSetting::MachinePreference("test"))
                        .map(|_| ())
                })
            })
            .collect::<Vec<_>>();
        for worker in workers {
            worker.join().unwrap()?;
        }
        Ok(())
    })
}

#[test]
fn typed_session_settings_resume_in_their_own_shard() -> Result<()> {
    use deepseek_code_core::{permissions::rules::PermissionAction, super_agent::Role};
    use settings::*;
    let root = tempdir()?;
    let workspace = root.path();
    let prefs = Preferences {
        effort: Some(Effort::High),
        autocompact_percent: Some(80.try_into()?),
        autocompact_off: false,
        goal: Some(CliGoal {
            text: "test".into(),
            completed: false,
            last_check: Some("pending".into()),
        }),
    };
    let rules = std::collections::HashMap::from([("Read".to_owned(), PermissionAction::Allow)]);
    let config = vec![None, Some("local settings".to_owned()), None];
    let requests: Vec<LiveRequest> = serde_json::from_value(
        json!([{"id":"r","startedAt":1234,"inputTokens":45,"prompt":"discard"}]),
    )?;
    let display = Display {
        diff_staged: true,
        ..Display::default()
    };
    let editor = Editor {
        port: 5100,
        workspace: workspace.into(),
    };
    let seen = vec![(workspace.join("AGENTS.md"), "digest".into())];
    let progress = json!({"status":"completed","output_tokens":14})
        .as_object()
        .unwrap()
        .clone();
    {
        let stores = Stores::open(workspace)?;
        let a = stores.create(A, Default::default())?;
        a.write(|w| {
            w.set_setting(PREFERENCES, &prefs)?;
            w.set_setting(PERMISSIONS, &rules)?;
            w.set_setting(SESSION_START_CONTEXT, &"hook context".into())?;
            w.set_setting(accepted_configuration(workspace), &config)?;
            w.set_setting(
                rejected_configuration(workspace),
                &(config.clone(), "denied".into()),
            )?;
            w.set_setting(SEEN_INSTRUCTIONS, &seen)?;
            w.set_setting(INSTRUCTIONS_BOUNDARY, &"boundary".into())?;
            w.set_setting(DISPLAY, &display)?;
            w.set_setting(EDITOR, &editor)?;
            w.set_setting(ROLE, &Role::Router)?;
            w.set_setting(AGENT_DESCRIPTION, &"worker".into())?;
            w.set_setting(AGENT_FORK_PARENT, &"source".into())?;
            w.set_setting(
                MICROCOMPACT,
                &Microcompact {
                    boundary: "m".into(),
                    through: 11,
                    cleared_results: 0,
                },
            )?;
            w.set_setting(CHILD_PROGRESS, &progress)?;
            w.set_setting(ENTERED_WORKTREE, &EnteredWorktree { created: false })?;
            w.set_setting(LIVE_REQUESTS, &requests)
        })?;
        assert!(a
            .write::<()>(|w| {
                w.clear_setting(PREFERENCES)?;
                bail!("roll back")
            })
            .is_err());
    }
    let stores = Stores::open(workspace)?;
    stores.open_session(A)?.read(|r| {
        assert_eq!(r.setting(PREFERENCES)?, Some(prefs));
        assert_eq!(r.setting(PERMISSIONS)?, Some(rules));
        assert_eq!(
            r.setting(SESSION_START_CONTEXT)?.as_deref(),
            Some("hook context")
        );
        assert_eq!(
            r.setting(accepted_configuration(workspace))?,
            Some(config.clone())
        );
        assert!(r
            .setting(accepted_configuration(&workspace.join("other")))?
            .is_none());
        assert_eq!(
            r.setting(rejected_configuration(workspace))?,
            Some((config, "denied".into()))
        );
        assert_eq!(r.setting(SEEN_INSTRUCTIONS)?, Some(seen));
        assert_eq!(
            r.setting(INSTRUCTIONS_BOUNDARY)?.as_deref(),
            Some("boundary")
        );
        assert_eq!(r.setting(DISPLAY)?, Some(display));
        assert_eq!(r.setting(EDITOR)?, Some(editor));
        assert_eq!(r.setting(AGENT_DESCRIPTION)?.as_deref(), Some("worker"));
        assert_eq!(r.setting(AGENT_FORK_PARENT)?.as_deref(), Some("source"));
        assert_eq!(r.setting(ROLE)?, Some(Role::Router));
        assert_eq!(r.setting(MICROCOMPACT)?.unwrap().through, 11);
        assert_eq!(r.setting(CHILD_PROGRESS)?, Some(progress));
        assert!(!r.setting(ENTERED_WORKTREE)?.unwrap().created);
        let accounting = r.setting(LIVE_REQUESTS)?.unwrap();
        assert_eq!(accounting, requests);
        assert!(serde_json::to_value(accounting)?[0].get("prompt").is_none());
        Ok(())
    })?;
    let b = stores.create("another", Default::default())?;
    assert!(b.read(|r| r.setting(PERMISSIONS))?.is_none());
    stores.open_session(A)?.write(|w| w.clear_setting(EDITOR))?;
    assert!(stores
        .open_session(A)?
        .read(|r| r.setting(EDITOR))?
        .is_none());
    Ok(())
}

#[test]
fn shard_replacement_with_the_same_incarnation_requires_verification() -> Result<()> {
    let root = tempdir()?;
    let stores = Stores::open(root.path())?;
    let session = stores.create(
        A,
        CreateOptions {
            first_message: Some(FirstMessage {
                origin: "typed".into(),
                text: "first".into(),
                typed: Some(json!(["test"])),
            }),
            ..Default::default()
        },
    )?;
    let dir = root.path().join("data/sessions").join(A);
    let path = dir.join("session.db");
    session.read(|r| r.initial_message())?;
    let key = std::fs::File::open(root.path().join("state/locks/session").join(A))?;
    key.lock()?;
    let replacement = dir.join("replacement.db");
    {
        let old = Connection::open(&path)?;
        let cookie: i64 = old.pragma_query_value(None, "schema_version", |r| r.get(0))?;
        old.execute("VACUUM INTO ?1", [replacement.to_str().unwrap()])?;
        let copy = Connection::open(&replacement)?;
        copy.execute_batch(
            "PRAGMA journal_mode=WAL;
            UPDATE message_user_content SET content_json=cast(content_json AS BLOB);
            UPDATE projection_state SET history_edit=NULL",
        )?;
        copy.pragma_update(None, "schema_version", cookie)?;
    }
    std::fs::rename(&replacement, &path)?;
    drop(key);
    assert!(session
        .read(|_| Ok(()))
        .unwrap_err()
        .to_string()
        .contains("decoding initial message"));
    Ok(())
}

#[test]
fn upgrading_p3_stamps_requires_rereading_and_binds_pending_requests() -> Result<()> {
    let root = tempdir()?;
    let path = root.path().join("tracked");
    std::fs::write(&path, b"first")?;
    let target = {
        let stores = Stores::open(root.path())?;
        let s = stores.create(A, Default::default())?;
        s.read_tracked_file(&path)?;
        s.write(|w| {
            w.append("user", "first")?;
            let target = w.append("assistant", "second")?;
            w.request_compaction("pending", 50, 100)?;
            Ok(target)
        })?
    };
    let db = root.path().join("data/sessions").join(A).join("session.db");
    {
        let c = Connection::open(&db)?;
        c.execute_batch("BEGIN IMMEDIATE")?;
        remove_v4_features(&c)?;
        c.execute_batch("PRAGMA user_version=3; COMMIT")?;
    }
    let stores = Stores::open(root.path())?;
    let s = stores.open_session(A)?;
    assert!(s.check_file_write(&path).is_err());
    s.read_tracked_file(&path)?;
    s.check_file_write(&path)?;
    let owner = s.delivery_owner()?;
    let claim = owner.claim_compaction()?.unwrap();
    s.write(|w| w.rewind_from(&target))?;
    assert_eq!(
        s.read(|r| r.compaction_cancellation(claim.request_id()))?
            .as_deref(),
        Some("target removed by rewind")
    );
    Ok(())
}
