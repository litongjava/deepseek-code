use anyhow::{ensure, Result};
use deepseek_code_core::db::{
    diff_repo::CodeDiffRow, message_repo::ToolCallRow, permission_repo::PermissionApprovalRow,
    session_store::*, turn_repo::TurnStatus,
};
use deepseek_code_core::services::{input_queue::QueueMode, subagents::Invocation};
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};
use std::{
    fs,
    path::Path,
    process::{Child, Command},
    thread,
    time::{Duration, Instant},
};
use tempfile::tempdir;

const A: &str = "10000000-1234-4234-9234-123456789abc";
const B: &str = "20000000-1234-4234-9234-123456789abc";
fn global(root: &Path) -> Result<Connection> {
    Ok(Connection::open_with_flags(
        root.join("global.db"),
        OpenFlags::SQLITE_OPEN_READ_WRITE,
    )?)
}
fn project(root: &Path) -> CreateOptions {
    CreateOptions {
        project: Some(ProjectSelection::Workspace(root.to_owned())),
        workspace_mode: Some("code".into()),
        ..Default::default()
    }
}
fn wire() -> Value {
    json!({"content":[{"type":"text","text":"durable reply"},{"type":"image","data":"YWJj","mimeType":"image/png"}],"_meta":{"trace":"actual save"}})
}
fn wait(path: &Path) -> Result<()> {
    let start = Instant::now();
    while !path.exists() {
        ensure!(
            start.elapsed() < Duration::from_secs(60),
            "Timeout waiting for {}",
            path.display()
        );
        thread::sleep(Duration::from_millis(10))
    }
    Ok(())
}
struct Worker(Child);
impl Worker {
    fn start(root: &Path, action: &str) -> Result<Self> {
        Ok(Self(
            Command::new(std::env::current_exe()?)
                .args(["--exact", "process_worker", "--ignored", "--nocapture"])
                .env("I46_P2_ROOT", root)
                .env("I46_P2_ACTION", action)
                .spawn()?,
        ))
    }
    fn kill(&mut self) -> Result<()> {
        self.0.kill()?;
        self.0.wait()?;
        Ok(())
    }
    fn finish(&mut self) -> Result<()> {
        let start = Instant::now();
        loop {
            if let Some(status) = self.0.try_wait()? {
                ensure!(status.success(), "worker exited {status}");
                return Ok(());
            }
            ensure!(start.elapsed() < Duration::from_secs(65), "worker stuck");
            thread::sleep(Duration::from_millis(10))
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
#[ignore = "child process entry point"]
fn process_worker() -> Result<()> {
    let root = std::path::PathBuf::from(std::env::var("I46_P2_ROOT")?);
    let action = std::env::var("I46_P2_ACTION")?;
    let control = root.clone();
    let hold = move || -> Result<()> {
        fs::write(control.join("ready"), [])?;
        wait(&control.join("release"))
    };
    if action == "block-global" {
        let c = global(&root)?;
        c.execute_batch("BEGIN IMMEDIATE")?;
        hold()?;
        c.execute_batch("ROLLBACK")?;
        return Ok(());
    }
    let s = Stores::open(&root)?;
    match action.as_str() {
        "dispatch" => {
            let mut owner = s.open_session(B)?.delivery_owner()?;
            owner.next_queue(QueueMode::NextTurn, None)?;
            let (_, turn) = owner.begin_generation()?.unwrap();
            fs::write(root.join("started-turn"), turn)?;
            hold()?;
        }
        "block-session" => s.open_session(A)?.write(|_| hold())?,
        "read-b" => {
            let display = fs::read_to_string(root.join("display"))?;
            let v = s
                .open_session(B)?
                .read(|r| r.mcp("assistant:mcp", &display))?
                .unwrap();
            assert_eq!(v.1["_meta"]["trace"], "actual save");
            assert_eq!(v.0.as_array().unwrap().len(), 2);
            assert!(!v.2);
        }
        _ => {
            let (kind, phase) = action.split_once('-').unwrap();
            let phase = match phase {
                "claim" => Phase::DeliveryClaimed,
                "commit" => Phase::DeliveryCommitted,
                "ack" => Phase::DeliveryAcknowledged,
                _ => unreachable!(),
            };
            let hooked = s.with_fault_hook(move |p| if p == phase { hold() } else { Ok(()) });
            let mut owner = hooked.open_session(B)?.delivery_owner()?;
            if kind == "peer" {
                owner.next_peer()?;
            } else if kind == "tool" {
                owner.next_queue(QueueMode::Steer, Some("frozen"))?;
            } else {
                owner.next_queue(QueueMode::NextTurn, None)?;
            }
        }
    }
    Ok(())
}

#[test]
fn admitted_mcp_result_is_durable_while_another_writer_remains_locked() -> Result<()> {
    for action in ["block-session", "block-global"] {
        let root = tempdir()?;
        let s = Stores::open(root.path())?;
        let a = s.create(A, Default::default())?;
        let b = s.create(B, Default::default())?;
        let mut worker = Worker::start(root.path(), action)?;
        wait(&root.path().join("ready"))?;
        let start = Instant::now();
        let contender = if action == "block-session" {
            Some(thread::spawn(move || {
                a.write(|w| w.append("user", "serialized"))
            }))
        } else {
            None
        };
        thread::sleep(Duration::from_millis(5150));
        let display = b.save_mcp("assistant", "mcp", &wire(), false)?;
        fs::write(root.path().join("display"), display)?;
        Worker::start(root.path(), "read-b")?.finish()?;
        assert!(worker.0.try_wait()?.is_none());
        if let Some(job) = &contender {
            assert!(!job.is_finished());
        }
        eprintln!(
            "{action}: {} ms before releasing writer; A={} B={} G={}",
            start.elapsed().as_millis(),
            root.path()
                .join(format!("data/sessions/{A}/session.db"))
                .display(),
            root.path()
                .join(format!("data/sessions/{B}/session.db"))
                .display(),
            root.path().join("global.db").display()
        );
        fs::write(root.path().join("release"), [])?;
        worker.finish()?;
        if let Some(job) = contender {
            job.join().unwrap()?;
        }
    }
    Ok(())
}

#[test]
fn killed_delivery_owners_recover_one_effect_and_one_dispatch() -> Result<()> {
    for kind in ["queue", "tool", "peer"] {
        for phase in ["claim", "commit", "ack"] {
            let root = tempdir()?;
            let s = Stores::open(root.path())?;
            let a = s.create(A, project(root.path()))?;
            let b = s.create(B, project(root.path()))?;
            if kind == "queue" {
                b.enqueue(
                    "request",
                    &json!("user intent"),
                    QueueMode::NextTurn,
                    deepseek_code_core::services::input_queue::QueueOrigin::User,
                )?;
            } else if kind == "tool" {
                b.write(|w| {
                    let m = w.append("assistant", "using tool")?;
                    w.tool(&tool("frozen", &m, 0), None)
                })?;
                b.enqueue(
                    "request",
                    &json!("tool intent"),
                    QueueMode::Steer,
                    deepseek_code_core::services::input_queue::QueueOrigin::User,
                )?;
            } else {
                a.send_peer(&b, "operation", "peer data")?;
            }
            let mut worker = Worker::start(root.path(), &format!("{kind}-{phase}"))?;
            wait(&root.path().join("ready"))?;
            worker.kill()?;
            let committed = phase != "claim";
            assert_eq!(b.read(|r| r.receipt_count())?, i64::from(committed));
            let mut owner = b.delivery_owner()?;
            let recovery = if kind == "queue" {
                owner.next_queue(QueueMode::NextTurn, None)?
            } else if kind == "tool" {
                // A replacement owner must use the target frozen by the original claim.
                owner.next_queue(QueueMode::Steer, None)?
            } else {
                owner.next_peer()?
            };
            assert_eq!(
                recovery.map(|h| h.applied),
                match phase {
                    "claim" => Some(true),
                    "commit" => Some(false),
                    _ => None,
                }
            );
            assert_eq!(b.read(|r| r.messages())?.len(), 1);
            assert_eq!(b.read(|r| r.receipt_count())?, 1);
            assert_eq!(
                b.read(|r| r.authority())?.len(),
                usize::from(kind != "peer")
            );
            assert_eq!(
                b.read(|r| r.delivered_inputs("frozen"))?.len(),
                usize::from(kind == "tool")
            );
            assert_eq!(owner.begin_generation()?.is_some(), kind == "queue");
            assert!(owner.begin_generation()?.is_none());
        }
    }
    Ok(())
}

fn tool(id: &str, message: &str, n: i64) -> ToolCallRow {
    ToolCallRow {
        id: id.into(),
        message_id: message.into(),
        name: "fixture".into(),
        input: Some("{}".into()),
        output: Some("result".into()),
        is_error: false,
        sort_order: n,
    }
}
#[test]
fn queue_revision_pause_tool_delivery_and_peer_revocation() -> Result<()> {
    let root = tempdir()?;
    let s = Stores::open(root.path())?;
    let a = s.create(A, project(root.path()))?;
    let b = s.create(B, project(root.path()))?;
    let q = b.enqueue(
        "unique",
        &json!("earlier"),
        QueueMode::Steer,
        deepseek_code_core::services::input_queue::QueueOrigin::User,
    )?;
    assert_eq!(
        b.enqueue(
            "unique",
            &json!("earlier"),
            QueueMode::Steer,
            deepseek_code_core::services::input_queue::QueueOrigin::User
        )?,
        q
    );
    assert!(b
        .enqueue(
            "unique",
            &json!("other"),
            QueueMode::Steer,
            deepseek_code_core::services::input_queue::QueueOrigin::User
        )
        .is_err());
    assert_eq!(b.queue_items()?[0].content, json!("earlier"));
    let edit = b.edit_queue(q.id, q.revision, QueueEdit::Text("edited"))?;
    assert!(b.edit_queue(q.id, q.revision, QueueEdit::Cancel).is_err());
    b.pause_queue(true)?;
    let mut owner = b.delivery_owner()?;
    assert!(owner.next_queue(QueueMode::Steer, Some("tool"))?.is_none());
    drop(owner);
    b.pause_queue(false)?;
    b.write(|w| {
        let m = w.append("assistant", "")?;
        w.tool(&tool("tool", &m, 0), None)
    })?;
    let mut owner = b.delivery_owner()?;
    assert!(
        owner
            .next_queue(QueueMode::Steer, Some("tool"))?
            .unwrap()
            .applied
    );
    assert!(owner.next_queue(QueueMode::Steer, Some("tool"))?.is_none());
    drop(owner);
    assert!(b
        .edit_queue(q.id, edit.revision, QueueEdit::Cancel)
        .is_err());
    assert_eq!(
        b.read(|r| r.delivered_inputs("tool"))?[0].content,
        json!("edited")
    );
    assert_eq!(
        a.send_peer(&b, "op", "quoted")?,
        a.send_peer(&b, "op", "quoted")?
    );
    assert!(a.send_peer(&b, "op", "conflict").is_err());
    let hooked = s.with_fault_hook(|p| {
        ensure!(p != Phase::DeliveryClaimed, "interrupt");
        Ok(())
    });
    assert!(hooked
        .open_session(B)?
        .delivery_owner()?
        .next_peer()
        .is_err());
    let envelope = &b.inbox(true)?[0];
    assert_eq!(envelope.content, "quoted");
    b.mark_peer_read(&envelope.id)?;
    assert!(b.inbox(true)?.is_empty());
    assert_eq!(a.sent()?[0].id, envelope.id);
    global(root.path())?.execute("UPDATE project_folders SET trusted=0", [])?;
    assert!(b.delivery_owner()?.next_peer()?.is_none());
    assert_eq!(b.read(|r| r.messages())?.len(), 1);
    Ok(())
}

#[test]
fn history_nested_replay_full_partial_clear_and_worker_address() -> Result<()> {
    let root = tempdir()?;
    let stores = Stores::open(root.path())?;
    let a = stores.create(A, Default::default())?;
    a.admit_user(
        "first",
        &json!([{"type":"text","text":"user scope"}]),
        "typed",
    )?;
    let (turn,m)=a.write(|w|{
        w.bind_agent("agent restrictions")?;let t=w.start_turn()?;let m=w.segment(&t)?;w.message_content(&m,"reply")?;
        let nested=format!("{m}:nested");
        w.tool(&tool("parent",&m,0),None)?;w.tool(&tool(&nested,&m,1),Some(("parent",1)))?;
        w.tool_duration(&nested,37)?;w.tool_note(&nested,"note")?;
        w.attachment("file",&m,"a.txt","file","text/plain",5,"reference")?;
        w.approval(&PermissionApprovalRow{id:"approval".into(),conversation_id:A.into(),message_id:m.clone(),tool_name:"tool".into(),action:"write".into(),risk_level:"medium".into(),status:"pending".into(),user_decision:None,decision_reason:None,created_at:"now".into(),decided_at:None})?;
        w.decide("approval","approved",Some("allow"),None,"later")?;
        w.diff(&CodeDiffRow{id:"diff".into(),conversation_id:A.into(),message_id:m.clone(),file_path:"x".into(),original_content:None,modified_content:Some("new".into()),diff_text:None,status:"pending".into(),applied_at:None,created_at:"now".into()})?;
        w.snapshots(&m,&json!([{"id":"req","model":"fixture","systemPrompt":"header","tools":[{"name":"one"}],"responseBlocks":[{"id":"nested","type":"tool_call"}]}]))?;
        w.finish_turn(&t,TurnStatus::Cancelled,17)?;Ok((t,m))})?;
    let display = a.save_mcp(&m, "nested", &wire(), true)?;
    assert_eq!(
        a.read(|r| r.turn(&turn))?.unwrap().status,
        TurnStatus::Cancelled
    );
    assert_eq!(
        a.read(|r| r.approval("approval"))?.unwrap().status,
        "approved"
    );
    let p = a.read(|r| r.projection())?;
    let g = global(root.path())?;
    assert_eq!(
        g.query_row("SELECT (message_count,content_revision,last_turn_status)=(?1,?2,'cancelled') FROM conversations WHERE id=?3", rusqlite::params![p.count, p.revision, A], |r| r
            .get::<_, bool>(0))?,
        true
    );
    let fork = a.branch(B, Branch::Full)?;
    let copied = fork.read(|r| r.messages())?;
    assert_eq!(copied.len(), 2);
    let msg = &copied[1].id;
    assert_ne!(msg, &m);
    fork.read(|r| {
        let tools = r.tools(msg)?;
        let nested = &tools[1].id;
        for format in [
            deepseek_code_core::native_engine::provider_manager::ApiFormat::OpenAI,
            deepseek_code_core::native_engine::provider_manager::ApiFormat::Anthropic,
        ] {
            let (history, _) = r.model_history(&format)?;
            let protocol = serde_json::to_string(&history)?;
            assert!(protocol.contains(&tools[0].id));
            assert!(!protocol.contains(nested));
        }
        assert_ne!(nested, "nested");
        assert_eq!(
            r.origins(msg)?,
            vec![(nested.clone(), tools[0].id.clone(), 1)]
        );
        assert_eq!(r.tool_duration(nested)?, Some(37));
        assert_eq!(r.tool_note(nested)?.as_deref(), Some("note"));
        assert_eq!(r.mcp(nested, &display)?.unwrap().2, true);
        assert_eq!(r.attachments(msg)?.len(), 1);
        assert_eq!(r.diffs(msg)?.len(), 1);
        let requests = r.requests(msg)?;
        assert_ne!(requests[0]["id"], "req");
        assert_eq!(requests[0]["responseBlocks"][0]["id"], nested.as_str());
        assert_eq!(requests[0]["systemPrompt"], "header");
        assert_eq!(r.authority()?.len(), 1);
        Ok(())
    })?;
    let partial = a.branch(
        "partial",
        Branch::Through {
            message: Some("first"),
            index: None,
        },
    )?;
    assert_eq!(partial.read(|r| r.messages())?.len(), 1);
    assert!(partial.read(|r| r.authority())?.is_empty());
    let cleared = a.branch("cleared", Branch::Clear)?;
    assert!(cleared.read(|r| r.messages())?.is_empty());
    assert_eq!(
        cleared.read(|r| r.binding())?.as_deref(),
        Some("agent restrictions")
    );
    assert!(a
        .branch(
            "invalid",
            Branch::Through {
                message: Some("absent"),
                index: None
            }
        )
        .is_err());
    assert!(stores.open_session("invalid").is_err());
    let child = a.branch("child", Branch::Child)?;
    let address = child.address();
    let worker = stores.resume_history(&address)?;
    worker.write(|w| w.append("assistant", "child only"))?;
    a.write(|w| {
        w.child_invocation(
            &address,
            &Invocation {
                parent_session_id: A.into(),
                parent_message_id: m.clone(),
                tool_use_id: "parent".into(),
                child_session_id: "child".into(),
                attempt_id: "try".into(),
                mode: "owned".into(),
                status: "done".into(),
                description: "fixture".into(),
            },
        )
    })?;
    assert_eq!(
        a.read(|r| r.child_invocations(&m))?[0].0,
        address.incarnation
    );
    assert_eq!(a.read(|r| r.messages())?.len(), 2);
    child.delete()?;
    stores.create("child", Default::default())?;
    assert!(stores.resume_history(&address).is_err());
    a.write(|w| w.rewind_from(&m))?;
    assert_eq!(a.read(|r| r.projection())?.count, 1);
    assert!(a.read(|r| r.mcp("nested", &display))?.is_none());
    Ok(())
}

#[test]
fn import_preview_replay_dedup_and_atomic_visibility() -> Result<()> {
    for phase in [Phase::Sealed, Phase::BeforePublishCommit] {
        let root = tempdir()?;
        let src = tempdir()?;
        let s = Stores::open(root.path())?;
        for id in ["a/one", "b/two"] {
            let json = json!({"id":id,"name":id,"created_at":"2020-01-01T00:00:00Z","updated_at":"2020-01-02T00:00:00Z","messages":[{"role":"assistant","content":"hello","timestamp":"2020-01-02T00:00:00Z"}]});
            fs::write(
                src.path().join(format!("{}.json", &id[..1])),
                serde_json::to_vec(&json)?,
            )?;
        }
        assert_eq!(
            s.import_json(src.path(), root.path(), true)?.imported.len(),
            2
        );
        assert!(!root.path().join("data/backups").exists());
        let stopped = s.with_fault_hook(move |p| {
            ensure!(p != phase, "interrupted");
            Ok(())
        });
        assert!(stopped.import_json(src.path(), root.path(), false).is_err());
        assert_eq!(
            global(root.path())?.query_row("SELECT count(*) FROM conversations", [], |r| r
                .get::<_, i64>(0))?,
            0
        );
        drop((s, stopped));
        let s = Stores::open(root.path())?;
        let n = global(root.path())?.query_row("SELECT count(*) FROM conversations", [], |r| {
            r.get::<_, usize>(0)
        })?;
        assert_eq!(n, if phase == Phase::Sealed { 0 } else { 2 });
        let result = s.import_json(src.path(), root.path(), false)?;
        assert_eq!(result.skipped.len(), n);
        if n == 0 {
            assert!(result.backup.unwrap().is_file())
        }
        for id in ["a/one", "b/two"] {
            assert_eq!(
                s.open_session(id)?.read(|r| r.messages())?[0].content,
                "hello"
            )
        }
        s.open_session("a/one")?.delete()?;
        assert!(s.import_json(src.path(), root.path(), false).is_err());
    }
    Ok(())
}

fn commit_native_fixture(session: &SessionDb, native: Value) -> Result<String> {
    session.write(|w| {
        w.append("user", "request")?;
        w.append("assistant", "answer")?;
        w.request_compaction("retain", 50, 100)
    })?;
    let owner = session.delivery_owner()?;
    owner.finish_compaction(
        &owner.claim_compaction()?.unwrap(),
        &deepseek_code_core::services::compaction::GeneratedSummary {
            text: "summary".into(),
            native: Some(native),
        },
    )
}

#[test]
fn fork_pins_one_snapshot_and_resets_local_compaction_count() -> Result<()> {
    use deepseek_code_core::native_engine::provider_manager::ApiFormat;
    use std::sync::{mpsc, Mutex};
    let root = tempdir()?;
    let stores = Stores::open(root.path())?;
    let a = stores.create(A, Default::default())?;
    let boundary = commit_native_fixture(&a, json!({"compaction":true,"model":"original"}))?;
    let (pinned, observed) = mpsc::sync_channel(1);
    let (release, wait) = mpsc::sync_channel(1);
    let wait = Mutex::new(wait);
    let copying = stores.with_fault_hook(move |phase| {
        if phase == Phase::ForkSnapshot {
            pinned.send(())?;
            wait.lock().unwrap().recv_timeout(Duration::from_secs(20))?;
        }
        Ok(())
    });
    let copy = thread::spawn(move || copying.open_session(A)?.branch(B, Branch::Full));
    observed.recv_timeout(Duration::from_secs(20))?;
    let late = a.write(|w| w.append("assistant", "after snapshot"));
    release.send(())?;
    let fork = copy.join().unwrap()?;
    late?;
    assert_eq!(a.read(|r| r.messages())?.len(), 4);
    assert_eq!(a.read(|r| r.compaction_count())?, 1);
    fork.read(|r| {
        assert_eq!(r.compaction_count()?, 0);
        assert_eq!(r.messages()?.len(), 3);
        let (history, id) = r.model_history(&ApiFormat::OpenAI)?;
        assert_ne!(id.as_ref(), Some(&boundary));
        assert_eq!(history.len(), 1);
        assert_eq!(history[0]["provider_content"]["model"], "original");
        Ok(())
    })
}

#[test]
fn streamed_occurrences_bind_typed_mcp_and_owned_workspace() -> Result<()> {
    let root = tempdir()?;
    let stores = Stores::open(root.path())?;
    let a = stores.create(
        A,
        CreateOptions {
            title: Some("CLI session".into()),
            ..project(root.path())
        },
    )?;
    let msg = a.write(|w| w.append("assistant", ""))?;
    let display = a.save_mcp(&msg, "raw", &wire(), false)?;
    a.write(|w| {
        let mut nested = tool("raw", &msg, 1);
        nested.output = Some(display.clone());
        let origins = std::collections::BTreeMap::from([("raw".into(), ("parent".into(), 1))]);
        w.stream_tools(&msg, "reply", &[tool("parent", &msg, 0), nested], &origins)?;
        w.snapshots(
            &msg,
            &json!([{"id":"trace","responseBlocks":[{"type":"tool_use","id":"raw"}]}]),
        )?;
        Ok(())
    })?;
    let qualified = format!("{msg}:raw");
    let origin = a.read(|r| r.origins(&msg))?;
    assert_eq!(origin[0].0, qualified);
    assert_eq!(
        a.read(|r| r.mcp(&qualified, &display))?.unwrap().0[0]["text"],
        "durable reply"
    );
    let fork = a.branch(B, Branch::Full)?;
    fork.read(|r| {
        let msg = &r.messages()?[0].id;
        let occurrence = &r.origins(msg)?[0].0;
        assert_eq!(
            r.requests(msg)?[0]["responseBlocks"][0]["id"],
            occurrence.as_str()
        );
        Ok(())
    })?;
    let other = tempdir()?;
    assert!(a.derive_owned_child("wrong", other.path()).is_err());
    assert!(stores.open_session("wrong").is_err());
    assert_eq!(
        a.derive_owned_child("correct", root.path())?.id(),
        "correct"
    );
    Ok(())
}

#[test]
fn projections_only_advance_and_identical_snapshots_leave_both_revisions_alone() -> Result<()> {
    let root = tempdir()?;
    let stores = Stores::open(root.path())?;
    let a = stores.create(A, Default::default())?;
    let request = json!([{"id":"r","systemPrompt":"same","model":"model"}]);
    let m = a.write(|w| {
        let m = w.append("assistant", "prefix")?;
        w.snapshots(&m, &request)?;
        Ok(m)
    })?;
    let current = a.read(|r| r.projection())?;
    let g = global(root.path())?;
    let version = || g.pragma_query_value(None, "data_version", |r| r.get::<_, i64>(0));
    let before = version()?;
    a.write(|w| w.snapshots(&m, &request))?;
    assert_eq!(a.read(|r| r.projection())?, current);
    assert_eq!(version()?, before);
    let newer = current.revision + 5;
    g.execute(
        "UPDATE conversations SET content_revision=?2,title='explicit title' WHERE id=?1",
        rusqlite::params![A, newer],
    )?;
    a.write(|w| w.message_content(&m, "later"))?;
    assert_eq!(
        g.query_row(
            "SELECT content_revision,title FROM conversations WHERE id=?1",
            [A],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        )?,
        (newer, "explicit title".to_owned())
    );
    Ok(())
}

#[test]
fn rewind_retires_undispatched_generation_and_fork_keeps_early_nested_origin() -> Result<()> {
    let root = tempdir()?;
    let stores = Stores::open(root.path())?;
    let a = stores.create(A, Default::default())?;
    a.enqueue(
        "request",
        &json!("withdrawn"),
        QueueMode::NextTurn,
        deepseek_code_core::services::input_queue::QueueOrigin::User,
    )?;
    let mut owner = a.delivery_owner()?;
    let effect = owner.next_queue(QueueMode::NextTurn, None)?.unwrap();
    a.write(|w| w.rewind_from(&effect.id))?;
    assert!(owner.begin_generation()?.is_none());
    assert_eq!(a.read(|r| r.receipt_count())?, 1);
    drop(owner);
    a.write(|w| {
        let m = w.append("assistant", "parent still streaming")?;
        w.tool(&tool("child", &m, 1), Some(("late-parent", 5)))
    })?;
    let fork = a.branch(B, Branch::Full)?;
    fork.read(|r| {
        let m = &r.messages()?[0].id;
        let origins = r.origins(m)?;
        assert_eq!(origins.len(), 1);
        assert_ne!(origins[0].1, "late-parent");
        let history = r
            .model_history(
                &deepseek_code_core::native_engine::provider_manager::ApiFormat::Anthropic,
            )?
            .0;
        assert!(!serde_json::to_string(&history)?.contains(&origins[0].0));
        Ok(())
    })
}

#[test]
fn private_child_never_resumes_in_a_reused_parents_workspace() -> Result<()> {
    let root = tempdir()?;
    let stores = Stores::open(root.path())?;
    let parent = stores.create("../private/owner", Default::default())?;
    let original = parent.workspace()?;
    let child = parent.derive_owned_child("owned", &original)?;
    let grandchild = child.derive_owned_child("also-owned", &original)?;
    assert_eq!(grandchild.workspace()?, original);
    parent.delete()?;
    let replacement = stores.create("../private/owner", Default::default())?;
    assert_ne!(replacement.workspace()?, original);
    assert!(child.workspace().is_err());
    assert!(grandchild.workspace().is_err());
    Ok(())
}

#[test]
fn accepted_p1_directory_and_shard_upgrade_without_changing_identity() -> Result<()> {
    let root = tempdir()?;
    let store = uuid::Uuid::new_v4().to_string();
    let inc = uuid::Uuid::new_v4().to_string();
    let seal = uuid::Uuid::new_v4().to_string();
    let sessions = root.path().join("data/sessions");
    for dir in [".staging", ".trash", ".quarantine", A] {
        fs::create_dir_all(sessions.join(dir))?
    }
    let locks = root.path().join("state/locks/session");
    fs::create_dir_all(&locks)?;
    for name in [".directory", ".orphan-sweep", A] {
        fs::write(locks.join(name), [])?
    }
    // Accepted P1's final bootstrap ticket, left after renaming G and before
    // publishing the namespace marker.
    fs::write(root.path().join(".session-bootstrap"), [])?;
    let staged = root.path().join(".global-staging");
    fs::create_dir(&staged)?;
    fs::write(staged.join("store-uuid"), &store)?;
    {
        let g = Connection::open(root.path().join("global.db"))?;
        g.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA application_id=0x44434c47; PRAGMA user_version=1",
        )?;
        g.execute_batch(include_str!("../src/db/session_store/global.sql"))?;
        g.execute("INSERT INTO store_identity VALUES(1,?1)", [&store])?;
        g.execute("INSERT INTO conversations(id,storage_key,incarnation,workspace_mode,created_at,updated_at,message_count,content_revision,transcript_bytes) VALUES(?1,?1,?2,'chat','now','now',1,1,3)",[A,&inc])?;
        let s = Connection::open(sessions.join(A).join("session.db"))?;
        s.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA application_id=0x44434c53; PRAGMA user_version=1",
        )?;
        s.execute_batch(include_str!("../src/db/session_store/session.sql"))?;
        s.execute(
            "INSERT INTO manifest VALUES(1,?1,?2,?3,?4,'now','now',1,1,NULL)",
            [&store, A, &inc, &seal],
        )?;
        s.execute("INSERT INTO messages(id,conversation_id,role,content,created_at,sort_order) VALUES('m',?1,'user','old','now',0)",[A])?;
    }
    let stores = Stores::open(root.path())?;
    let a = stores.open_session(A)?;
    assert_eq!(a.address().incarnation, inc);
    assert_eq!(a.read(|r| r.messages())?[0].content, "old");
    let (legacy, _) = a.read(|r| {
        r.model_history(&deepseek_code_core::native_engine::provider_manager::ApiFormat::OpenAI)
    })?;
    assert!(legacy[0].get("origin").is_none());
    // The v10 migration publishes the refreshed prompt projection once.
    assert_eq!(a.read(|r| r.projection())?.revision, 2);
    a.save_mcp("assistant", "new", &wire(), false)?;
    assert_eq!(a.read(|r| r.projection())?.revision, 3);
    assert_eq!(fs::read_to_string(locks.join(".initialized"))?, store);
    Ok(())
}

#[test]
fn simultaneous_inbox_consumers_commit_each_generation_in_fifo_order() -> Result<()> {
    use std::sync::{Arc, Barrier};
    let root = tempdir()?;
    let stores = Stores::open(root.path())?;
    let a = stores.create(A, project(root.path()))?;
    let b = stores.create(B, project(root.path()))?;
    let g = global(root.path())?;
    let size = || {
        g.query_row(
            "SELECT page_count*page_size FROM pragma_page_count,pragma_page_size",
            [],
            |r| r.get::<_, usize>(0),
        )
    };
    let baseline = size()?;
    let mut sent_a = Vec::new();
    let mut sent_b = Vec::new();
    for index in 0..16 {
        sent_a.push(b.send_peer(&a, &format!("to-a-{index}"), "peer message")?);
        sent_b.push(a.send_peer(&b, &format!("to-b-{index}"), "peer message")?);
    }
    let start = Arc::new(Barrier::new(2));
    let consume = |s: SessionDb, ids: Vec<String>, start: Arc<Barrier>| -> Result<Vec<u128>> {
        start.wait();
        let mut owner = s.delivery_owner()?;
        let mut durations = Vec::new();
        for id in ids {
            let now = Instant::now();
            let effect = owner.next_peer()?.unwrap();
            durations.push(now.elapsed().as_micros());
            assert!(effect.applied);
            assert_eq!(effect.id, id);
        }
        assert!(owner.next_peer()?.is_none());
        Ok(durations)
    };
    let other = start.clone();
    let (one, two) = thread::scope(|scope| {
        let one = scope.spawn(|| consume(a, sent_a, other));
        let two = scope.spawn(|| consume(b, sent_b, start));
        (one.join().unwrap(), two.join().unwrap())
    });
    let mut times = one?;
    times.extend(two?);
    times.sort();
    let after = size()?;
    eprintln!("concurrent inbox handoff: samples={} p95={}us p99={}us, G logical bytes for 2 sessions before admission={} after 32 acknowledged messages={}",times.len(),times[30],times[31],baseline,after);
    Ok(())
}

#[test]
fn killed_executor_settles_its_started_turn_and_never_redispatches_it() -> Result<()> {
    let root = tempdir()?;
    let stores = Stores::open(root.path())?;
    let b = stores.create(B, Default::default())?;
    b.enqueue(
        "launch",
        &json!("work"),
        QueueMode::NextTurn,
        deepseek_code_core::services::input_queue::QueueOrigin::User,
    )?;
    let mut worker = Worker::start(root.path(), "dispatch")?;
    wait(&root.path().join("ready"))?;
    let turn = fs::read_to_string(root.path().join("started-turn"))?;
    assert_eq!(
        b.read(|r| r.turn(&turn))?.unwrap().status,
        TurnStatus::Running
    );
    worker.kill()?;
    let mut owner = b.delivery_owner()?;
    assert!(owner.begin_generation()?.is_none());
    let interrupted = b.read(|r| r.turn(&turn))?.unwrap();
    assert_eq!(interrupted.status, TurnStatus::Cancelled);
    assert!(interrupted.ended_at.is_none());
    assert_eq!(b.read(|r| r.receipt_count())?, 1);
    Ok(())
}

#[test]
fn review_sender_retirement_preserves_recipient_outcome() -> Result<()> {
    for cut in [Phase::DeliveryClaimed, Phase::DeliveryCommitted] {
        let root = tempdir()?;
        let s = Stores::open(root.path())?;
        let a = s.create(A, project(root.path()))?;
        let b = s.create(B, project(root.path()))?;
        let id = a.send_peer(&b, "op", "peer data")?;
        let stop = s.with_fault_hook(move |phase| {
            ensure!(phase != cut, "cut");
            Ok(())
        });
        assert!(stop.open_session(B)?.delivery_owner()?.next_peer().is_err());
        a.delete()?;
        let replacement = s.create(A, project(root.path()))?;
        assert!(replacement.sent()?.is_empty());
        let result = b.delivery_owner()?.next_peer()?;
        let inbox = b.inbox(false)?;
        assert_eq!(inbox.len(), 1);
        assert_eq!(inbox[0].id, id);
        if cut == Phase::DeliveryClaimed {
            assert!(result.is_none());
            assert_eq!(inbox[0].state, "undeliverable");
            assert!(inbox[0].blocked_reason.is_some());
            assert_eq!(b.read(|r| r.receipt_count())?, 0);
        } else {
            assert!(!result.unwrap().applied);
            assert_eq!(inbox[0].state, "context_committed");
            assert_eq!(b.read(|r| r.messages())?.len(), 1);
        }
    }
    Ok(())
}

#[test]
fn populated_v2_inbox_upgrades_with_outstanding_claim() -> Result<()> {
    let root = tempdir()?;
    {
        let stores = Stores::open(root.path())?;
        let sender = stores.create(A, project(root.path()))?;
        let recipient = stores.create(B, project(root.path()))?;
        sender.send_peer(&recipient, "operation", "accepted")?;
        let stopped = stores.with_fault_hook(|p| {
            ensure!(p != Phase::DeliveryClaimed, "cut");
            Ok(())
        });
        assert!(stopped
            .open_session(B)?
            .delivery_owner()?
            .next_peer()
            .is_err());
    }
    // Rebuild the two changed tables with the original v2 definitions and
    // leave the old claim, payload digest and consumer generation in place.
    {
        let g = global(root.path())?;
        g.execute_batch(
            "BEGIN IMMEDIATE;
            ALTER TABLE session_inbox_operations RENAME TO operations_fixture;
            ALTER TABLE session_inbox RENAME TO inbox_fixture;
            DROP INDEX inbox_fifo;",
        )?;
        let schema = include_str!("../src/db/session_store/delivery.sql");
        let inbox = &schema[schema.find("CREATE TABLE session_inbox (").unwrap()
            ..schema.find("CREATE TABLE session_inbox_consumers").unwrap()];
        let ops = &schema[schema
            .find("CREATE TABLE session_inbox_operations")
            .unwrap()
            ..schema.find("CREATE TABLE legacy_session_imports").unwrap()];
        g.execute_batch(inbox)?;
        g.execute_batch(ops)?;
        g.execute_batch("INSERT INTO session_inbox SELECT id,sender,recipient,project_source,payload,size,created_at,state,generation,read,blocked_reason,committed_at FROM inbox_fixture;
            INSERT INTO session_inbox_operations SELECT * FROM operations_fixture;
            DROP TABLE operations_fixture; DROP TABLE inbox_fixture;
            DROP TABLE global_settings;
            ALTER TABLE conversations DROP COLUMN request_count;
            ALTER TABLE conversations DROP COLUMN input_tokens;
            ALTER TABLE conversations DROP COLUMN output_tokens;
            ALTER TABLE conversations DROP COLUMN total_tokens;
            PRAGMA user_version=2; COMMIT;")?;
    }
    let stores = Stores::open(root.path())?;
    let a = stores.open_session(A)?;
    let b = stores.open_session(B)?;
    let id = a.send_peer(&b, "operation", "accepted")?;
    assert_eq!(b.inbox(false)?[0].state, "claimed");
    assert_eq!(b.delivery_owner()?.next_peer()?.unwrap().id, id);
    a.delete()?;
    assert_eq!(b.inbox(false)?[0].state, "context_committed");
    Ok(())
}

#[test]
fn review_frozen_rewound_tool_cancels_and_unblocks_fifo() -> Result<()> {
    let root = tempdir()?;
    let s = Stores::open(root.path())?;
    let b = s.create(B, Default::default())?;
    let msg = b.write(|w| {
        let m = w.append("assistant", "")?;
        w.tool(&tool("tool", &m, 0), None)?;
        Ok(m)
    })?;
    for key in ["one", "two"] {
        b.enqueue(
            key,
            &json!(key),
            QueueMode::Steer,
            deepseek_code_core::services::input_queue::QueueOrigin::User,
        )?;
    }
    let stop = s.with_fault_hook(|p| {
        ensure!(p != Phase::DeliveryClaimed, "cut");
        Ok(())
    });
    assert!(stop
        .open_session(B)?
        .delivery_owner()?
        .next_queue(QueueMode::Steer, Some("tool"))
        .is_err());
    b.write(|w| w.rewind_from(&msg))?;
    let mut owner = b.delivery_owner()?;
    assert!(!owner.next_queue(QueueMode::Steer, None)?.unwrap().applied);
    assert_eq!(
        b.queue_items()?[0].state,
        deepseek_code_core::services::input_queue::QueueState::Cancelled
    );
    assert!(owner.next_queue(QueueMode::Steer, None)?.unwrap().applied);
    assert_eq!(b.read(|r| r.receipt_count())?, 1);
    Ok(())
}

#[test]
fn review_running_turn_consumes_steer_once() -> Result<()> {
    let root = tempdir()?;
    let b = Stores::open(root.path())?.create(B, Default::default())?;
    let mut owner = b.delivery_owner()?;
    let turn = b.write(|w| w.start_turn())?;
    for key in ["one", "two"] {
        b.enqueue(
            key,
            &json!(key),
            QueueMode::Steer,
            deepseek_code_core::services::input_queue::QueueOrigin::User,
        )?;
        assert!(owner.next_queue(QueueMode::Steer, None)?.unwrap().applied);
    }
    b.write(|w| w.finish_turn(&turn, TurnStatus::Completed, 10))?;
    assert!(owner.begin_generation()?.is_none());
    assert_eq!(b.read(|r| r.receipt_count())?, 2);
    assert_eq!(b.read(|r| r.messages())?.len(), 2);
    Ok(())
}

#[test]
fn review_edit_classifies_conflict_before_attachments() -> Result<()> {
    use deepseek_code_core::services::input_queue::QueueError;
    let root = tempdir()?;
    let s = Stores::open(root.path())?;
    let b = s.create(B, Default::default())?;
    let input = json!([{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw0KGgo="}}]);
    let item = b.enqueue(
        "image",
        &input,
        QueueMode::NextTurn,
        deepseek_code_core::services::input_queue::QueueOrigin::User,
    )?;
    let edit = |id, rev| {
        b.edit_queue(id, rev, QueueEdit::Text("edit"))
            .unwrap_err()
            .downcast::<QueueError>()
            .unwrap()
    };
    assert_eq!(
        edit(item.id, item.revision),
        QueueError::AttachmentsReadOnly
    );
    let stop = s.with_fault_hook(|p| {
        ensure!(p != Phase::DeliveryClaimed, "cut");
        Ok(())
    });
    assert!(stop
        .open_session(B)?
        .delivery_owner()?
        .next_queue(QueueMode::NextTurn, None)
        .is_err());
    assert_eq!(edit(item.id, item.revision), QueueError::Conflict);
    assert_eq!(
        edit(item.id, b.queue_items()?[0].revision),
        QueueError::Conflict
    );
    assert_eq!(edit(999, 1), QueueError::NotFound);
    Ok(())
}

#[test]
fn review_partial_fork_excludes_later_parent_and_sidecars() -> Result<()> {
    let root = tempdir()?;
    let a = Stores::open(root.path())?.create(A, Default::default())?;
    let (m, last) = a.write(|w| {
        let first = w.append("assistant", "nested")?;
        let last = w.append("assistant", "later")?;
        let parent = format!("{last}:parent");
        w.tool(&tool("child", &first, 0), Some((&parent, 1)))?;
        w.tool(&tool(&parent, &last, 0), None)?;
        w.tool_note(&parent, "later note")?;
        w.tool_duration(&parent, 9)?;
        Ok((first, last))
    })?;
    a.save_mcp(&last, "parent", &wire(), false)?;
    let fork = a.branch(
        B,
        Branch::Through {
            message: Some(&m),
            index: None,
        },
    )?;
    fork.read(|r| {
        let m = &r.messages()?[0].id;
        assert_eq!(r.tools(m)?.len(), 1);
        assert!(r.origins(m)?.is_empty());
        Ok(())
    })?;
    let db = Connection::open(root.path().join("data/sessions").join(B).join("session.db"))?;
    for table in ["tool_call_timings", "tool_call_notes", "mcp_tool_outputs"] {
        assert_eq!(
            db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r
                .get::<_, usize>(0))?,
            0
        );
    }
    Ok(())
}

#[test]
fn review_fork_rewrites_retry_and_compaction_references() -> Result<()> {
    let root = tempdir()?;
    let a = Stores::open(root.path())?.create(A, Default::default())?;
    let boundary = commit_native_fixture(&a, json!({}))?;
    a.write(|w|{
        let m=w.append("assistant","response")?;
        w.snapshots(&m,&json!([{"id":"retry","replacesRequestId":"original","contextBoundary":boundary},{"id":"original"}]))
    })?;
    a.branch(B, Branch::Full)?.read(|r| {
        let rows = r.messages()?;
        let requests = r.requests(&rows[3].id)?;
        assert_eq!(requests[0]["contextBoundary"], rows[2].id);
        assert_eq!(requests[0]["replacesRequestId"], requests[1]["id"]);
        assert_ne!(requests[1]["id"], "original");
        Ok(())
    })
}

#[test]
fn review_recovery_quarantines_unreadable_later_message() -> Result<()> {
    let root = tempdir()?;
    let s = Stores::open(root.path())?;
    let a = s.create(A, Default::default())?;
    a.write(|w| {
        w.append("user", "first")?;
        w.append("assistant", "second")
    })?;
    let stop = s.with_fault_hook(|p| {
        ensure!(p != Phase::Sealed, "cut");
        Ok(())
    });
    assert!(stop.open_session(A)?.branch(B, Branch::Full).is_err());
    let stage = fs::read_dir(root.path().join("data/sessions/.staging"))?
        .next()
        .unwrap()?
        .path();
    Connection::open(stage.join("session.db"))?.execute(
        "UPDATE messages SET thinking=CAST('bad' AS BLOB) WHERE sort_order=1",
        [],
    )?;
    drop((stop, a, s));
    let again = Stores::open(root.path())?;
    assert!(again.open_session(B).is_err());
    assert!(again.global().conversation(B)?.is_none());
    assert!(fs::read_dir(root.path().join("data/sessions/.quarantine"))?
        .next()
        .is_some());
    assert_eq!(again.open_session(A)?.read(|r| r.messages())?.len(), 2);
    Ok(())
}

#[test]
fn review_import_keeps_selected_workspace_across_backup() -> Result<()> {
    use deepseek_code_core::db::project_repo;
    let root = tempdir()?;
    let s = Stores::open(root.path())?;
    let old = tempdir()?;
    let new = tempdir()?;
    s.create(A, project(old.path()))?;
    let id = s.global().conversation(A)?.unwrap().project_id.unwrap();
    let input = root.path().join("in.json");
    fs::write(
        &input,
        serde_json::to_vec(
            &json!({"id":B,"name":"import","created_at":"2000-01-01T00:00:00Z","updated_at":"2000-01-01T00:00:00Z","messages":[]}),
        )?,
    )?;
    let mut g = global(root.path())?;
    let tx = g.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    let path = new.path().to_string_lossy().into_owned();
    project_repo::set_folders(&tx, &id, std::slice::from_ref(&path))?;
    project_repo::trust_folders(&tx, &id, &[path])?;
    thread::scope(|scope| -> Result<()> {
        let job = scope.spawn(|| s.import_json_into_project(&input, old.path(), false, Some(&id)));
        let start = Instant::now();
        while !fs::read_dir(root.path().join("data/backups"))
            .into_iter()
            .flatten()
            .flatten()
            .any(|e| e.path().join("manifest.json").exists())
        {
            if start.elapsed() > Duration::from_secs(30) {
                drop(tx);
                anyhow::bail!("import did not reach backup")
            }
            thread::sleep(Duration::from_millis(10));
        }
        tx.commit()?;
        assert!(job.join().unwrap().is_err());
        Ok(())
    })?;
    assert!(s.open_session(B).is_err());
    Ok(())
}

#[test]
fn review_activity_is_monotonic_in_both_stores() -> Result<()> {
    let root = tempdir()?;
    let s = Stores::open(root.path())?;
    let input = root.path().join("import.json");
    let future = "2099-01-01T00:00:00Z";
    fs::write(
        &input,
        serde_json::to_vec(
            &json!({"id":A,"name":"future","created_at":"2000-01-01T00:00:00Z","updated_at":future,"messages":[]}),
        )?,
    )?;
    s.import_json(&input, root.path(), false)?;
    let a = s.open_session(A)?;
    let g = global(root.path())?;
    g.execute(
        "UPDATE conversations SET updated_at='2100-01-01T00:00:00Z'",
        [],
    )?;
    a.write(|w| w.append("user", "hi"))?;
    assert_eq!(a.read(|r| r.projection())?.activity, future);
    assert_eq!(
        s.global().conversation(A)?.unwrap().updated_at,
        "2100-01-01T00:00:00Z"
    );
    assert_eq!(s.global().conversation(A)?.unwrap().message_count, 1);
    g.execute(
        "UPDATE conversations SET content_revision=100, updated_at='2099-01-01T00:30:00+01:00'",
        [],
    )?;
    a.write(|w| w.append("assistant", "later revision"))?;
    let row = s.global().conversation(A)?.unwrap();
    assert_eq!(row.updated_at, future);
    assert_eq!(row.message_count, 1);
    Ok(())
}

#[test]
fn review_directory_displays_clear_and_child_titles() -> Result<()> {
    let root = tempdir()?;
    let s = Stores::open(root.path())?;
    let a = s.create(A, Default::default())?;
    for (id, kind) in [("clear", Branch::Clear), ("child", Branch::Child)] {
        let session = a.branch(id, kind)?;
        assert_eq!(
            s.global().conversation(id)?.unwrap().title.as_deref(),
            Some("CLI session")
        );
        let prompt = format!("{}\n  first line\nsecond", " ".repeat(90));
        session.write(|w| w.append("user", &prompt))?;
        assert_eq!(
            s.global().conversation(id)?.unwrap().title.as_deref(),
            Some("first line")
        );
    }
    Ok(())
}

#[test]
fn review_rewind_removes_whole_tied_group() -> Result<()> {
    let root = tempdir()?;
    let a = Stores::open(root.path())?.create(A, Default::default())?;
    let m = a.write(|w| {
        w.append("user", "first")?;
        let m = w.append("assistant", "second")?;
        w.append("user", "last")?;
        Ok(m)
    })?;
    Connection::open(root.path().join("data/sessions").join(A).join("session.db"))?.execute(
        "UPDATE messages SET sort_order=CASE WHEN sort_order<2 THEN 8 ELSE 20 END",
        [],
    )?;
    a.write(|w| w.rewind_from(&m))?;
    assert!(a.read(|r| r.messages())?.is_empty());
    Ok(())
}

#[test]
fn review_unchanged_stream_preserves_each_occurrences_mcp_metadata() -> Result<()> {
    let root = tempdir()?;
    let a = Stores::open(root.path())?.create(A, Default::default())?;
    let stream = |m: &str| {
        a.write(|w| {
            let mut row = tool("provider", m, 0);
            row.output = Some("same".into());
            w.stream_tools(m, "reply", &[row], &Default::default())
        })
    };
    let mut messages = Vec::new();
    for stamp in ["first", "second"] {
        let m = a.write(|w| w.append("assistant", "reply"))?;
        a.save_mcp(
            &m,
            "provider",
            &json!({"content":[{"type":"text","text":"same"}],"_meta":{"stamp":stamp}}),
            false,
        )?;
        stream(&m)?;
        messages.push(m);
    }
    stream(&messages[0])?;
    for (m, stamp) in messages.iter().zip(["first", "second"]) {
        assert_eq!(
            a.read(|r| r.mcp(&format!("{m}:provider"), "same"))?
                .unwrap()
                .1["_meta"]["stamp"],
            stamp
        );
    }
    Ok(())
}

#[test]
fn round2_delayed_stream_and_transport_replay_keep_occurrence_results() -> Result<()> {
    let root = tempdir()?;
    let a = Stores::open(root.path())?.create(A, Default::default())?;
    let first = a.write(|w| w.append("assistant", "one"))?;
    let second = a.write(|w| w.append("assistant", "two"))?;
    let send = |message: &str, owner: &str| -> Result<()> {
        let wire = json!({"content":[{"type":"text","text":"done"}],
                          "structuredContent":{"owner":owner},"_meta":{"stamp":owner}});
        assert_eq!(a.save_mcp(message, "call", &wire, false)?, "done");
        Ok(())
    };
    let stream = |m: &str| {
        a.write(|w| {
            let mut row = tool("call", m, 0);
            row.output = Some("done".into());
            w.stream_tools(m, "answer", &[row], &Default::default())
        })
    };
    let check = |m: &str, owner: &str| -> Result<()> {
        let (content, metadata, failed) = a.read(|r| r.mcp(&format!("{m}:call"), "done"))?.unwrap();
        assert_eq!(content, json!({"owner":owner}).to_string());
        assert_eq!(
            metadata,
            json!({"structuredContent":{"owner":owner},"_meta":{"stamp":owner}})
        );
        assert!(!failed);
        Ok(())
    };
    send(&first, "A")?;
    send(&second, "B")?;
    stream(&second)?;
    stream(&first)?;
    check(&first, "A")?;
    check(&second, "B")?;
    send(&first, "A corrected")?;
    stream(&first)?;
    check(&first, "A corrected")?;
    send(&first, "A")?;
    stream(&first)?;
    stream(&second)?;
    check(&first, "A")?;
    check(&second, "B")
}

#[test]
fn round2_partial_and_full_forks_preserve_snapshot_shapes() -> Result<()> {
    let root = tempdir()?;
    let a = Stores::open(root.path())?.create(A, Default::default())?;
    let object = json!({"id":"request-1","status":"completed"});
    let opaque = json!([true, null, 12, "text", ["nested"], {"unchanged":true}]);
    let (first, later) = a.write(|w| {
        let first = w.append("user", "selected")?;
        let later = w.append("assistant", "later")?;
        w.snapshots(&later, &object)?;
        let mixed = w.append("assistant", "mixed")?;
        w.snapshots(&mixed, &opaque)?;
        let scalar = w.append("assistant", "scalar")?;
        w.snapshots(&scalar, &json!("scalar"))?;
        let retry = w.append("assistant", "retry")?;
        w.snapshots(
            &retry,
            &json!([{"id":"retry","replacesRequestId":"request-1"}]),
        )?;
        Ok((first, later))
    })?;
    let partial = a.branch(
        "partial",
        Branch::Through {
            message: Some(&first),
            index: None,
        },
    )?;
    assert_eq!(partial.read(|r| r.messages())?.len(), 1);
    a.branch(B, Branch::Full)?.read(|r| {
        let rows = r.messages()?;
        assert_eq!(rows.len(), 5);
        let request = r.requests(&rows[1].id)?;
        let new_id = request["id"].as_str().unwrap();
        assert_ne!(new_id, "request-1");
        assert_eq!(request, json!({"id":new_id,"status":"completed"}));
        assert_eq!(r.requests(&rows[2].id)?, opaque);
        assert_eq!(r.requests(&rows[3].id)?, "scalar");
        assert_eq!(r.requests(&rows[4].id)?[0]["replacesRequestId"], new_id);
        Ok(())
    })?;
    let shard = |id: &str| {
        Connection::open(
            root.path()
                .join("data/sessions")
                .join(id)
                .join("session.db"),
        )
    };
    for id in [A, B] {
        let anonymous: usize = shard(id)?.query_row(
            "SELECT count(*) FROM model_requests WHERE id IS NULL",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(anonymous, opaque.as_array().unwrap().len());
    }
    // A partial branch must never decode a trajectory beyond its boundary.
    shard(A)?.execute(
        "UPDATE message_trajectory SET requests_json='{' WHERE message_id=?1",
        [&later],
    )?;
    let prefix = a.branch(
        "prefix",
        Branch::Through {
            message: Some(&first),
            index: None,
        },
    )?;
    assert_eq!(prefix.read(|r| r.messages())?.len(), 1);
    Ok(())
}

#[test]
fn host_execution_lease_is_shared_for_handoff_and_fenced_to_the_shard() -> Result<()> {
    use std::sync::Arc;
    let root=tempdir()?;
    let stores=Stores::open(root.path())?;
    let source=stores.create("arbitrary/host:id",Default::default())?;
    let other=stores.create("other",Default::default())?;
    let lease=Arc::new(source.acquire_execution()?);
    assert!(source.acquire_execution().is_err());
    assert!(other.delivery_under_lease(lease.clone()).is_err());
    let mut boundary = source.delivery_under_lease(lease.clone())?;
    source.enqueue(
        "r",
        &json!("queued"),
        QueueMode::NextTurn,
        deepseek_code_core::services::input_queue::QueueOrigin::User,
    )?;
    assert!(
        boundary
            .next_queue(QueueMode::NextTurn, None)?
            .unwrap()
            .applied
    );
    assert!(boundary.begin_generation()?.is_some());
    drop(boundary);
    assert!(source.acquire_execution().is_err());
    drop(lease);
    assert_eq!(source.settle_abandoned(&source.acquire_execution()?)?.len(),1);
    Ok(())
}

#[tokio::test]
async fn task_get_parent_admission_uses_the_wait_budget() -> Result<()> {
    use std::time::{Duration, Instant};
    use deepseek_code_core::tools::tasks;
    let root = tempfile::tempdir()?;
    let stores = Stores::open(root.path())?;
    let parent_id = uuid::Uuid::new_v4().to_string();
    let child_id = uuid::Uuid::new_v4().to_string();
    let parent = stores.create(&parent_id, Default::default())?;
    let child = stores.create(&child_id, Default::default())?;
    child.write(|w| w.append("assistant", "committed report"))?;
    let job = uuid::Uuid::new_v4().to_string();
    let directory = root.path().join("tasks/background").join(&job);
    std::fs::create_dir_all(&directory)?;
    std::fs::write(
        directory.join("state.json"),
        json!({
            "id":job,"parent_session_id":parent_id,"session_id":child_id,
            "status":"completed","title":"fixture","started_ms":1,"heartbeat_ms":1
        })
        .to_string(),
    )?;
    std::fs::write(
        directory.join("session-owners.json"),
        json!({
            "parent":parent.incarnation(),"execution":child.incarnation()
        })
        .to_string(),
    )?;
    let key = std::fs::File::open(root.path().join("state/locks/session").join(&parent_id))?;
    key.lock()?;
    let (release, rx) = std::sync::mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        let _ = rx.recv_timeout(Duration::from_secs(3));
        drop(key);
    });
    let start = Instant::now();
    let response = tasks::execute_for_session_async("TaskGet",
        json!({"task_id":job,"block":true,"timeout":0}), root.path(), &parent_id).await;
    let elapsed = start.elapsed();
    drop(release);
    holder.join().unwrap();
    assert!(elapsed < Duration::from_millis(750), "{elapsed:?}");
    let response = response?;
    if response["retrieval_status"] != "timeout" {
        assert_eq!(response["stdout"], "committed report");
    }
    Ok(())
}
