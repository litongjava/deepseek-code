use super::*;
#[cfg(test)]
use crate::fixture_sql::FixtureSql;
use deepseek_code_core::services::session_messages::ExecutionLease;
use serde_json::json;

struct Fixture {
    _dir: tempfile::TempDir,
    context: deepseek_code_core::services::ServiceContext,
    path: std::path::PathBuf,
    job: background_sessions::Job,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(dir.path().join("data")).unwrap();
        let context = crate::services::context(&paths).unwrap();
        let store = deepseek_code_core::runtime::SessionStore::new(context.db.clone());
        let parent = store.create(dir.path(), "test", "cli-runtime").unwrap();
        let child = store.create(dir.path(), "test", "cli-runtime").unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let path = paths
            .data_dir
            .join("tasks/background")
            .join(&id)
            .join("state.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        context.db.directory_sql(|c|c.execute("INSERT INTO background_agents(job,parent_incarnation,child_incarnation,agent_type,description,started_at) SELECT ?1,p.incarnation,c.incarnation,'general','ACTIVE_AGENT_SENTINEL',datetime('now') FROM conversations p,conversations c WHERE p.id=?2 AND c.id=?3", (&id,&parent,&child))).unwrap().unwrap();
        context.db.seed_preference(&child, deepseek_code_core::db::session_store::settings::CHILD_PROGRESS, &json!({"parent_session_id":parent,"status":"running","description":"ACTIVE_AGENT_SENTINEL"})).unwrap();
        let owners = json!({
            "parent":store.db.open_session(&parent).unwrap().incarnation(),
            "execution":store.db.open_session(&child).unwrap().incarnation()
        });
        std::fs::write(
            path.with_file_name("session-owners.json"),
            serde_json::to_vec(&owners).unwrap(),
        )
        .unwrap();
        let now = chrono::Utc::now().timestamp_millis() as u64;
        let fixture = Self {
            _dir: dir,
            context,
            path,
            job: background_sessions::Job {
                id,
                foreground: false,
                parent_session_id: parent,
                session_id: child,
                title: "Agent: ACTIVE_AGENT_SENTINEL".into(),
                status: "running".into(),
                supervisor_pid: None,
                child_pid: None,
                started_ms: now - 3_661_000,
                heartbeat_ms: now,
                finished_ms: None,
                exit_code: None,
                error: None,
                launched: true,
            },
        };
        fixture.save();
        fixture
    }

    fn save(&self) {
        std::fs::write(&self.path, serde_json::to_vec(&self.job).unwrap()).unwrap();
    }

    fn agents(&self) -> Vec<Agent> {
        running_agents(
            &self.context.paths,
            &self.context.db,
            &self.job.parent_session_id,
        )
    }

    fn foreground(&self, parent: &SessionDb) -> (HistoryAddress, ExecutionLease) {
        use deepseek_code_core::db::session_store::settings::{AGENT_DESCRIPTION, CHILD_PROGRESS};
        use deepseek_code_core::services::subagents::Invocation;

        let db = &self.context.db;
        let store = deepseek_code_core::runtime::SessionStore::new(db.clone());
        let id = store
            .create(self._dir.path(), "test", "cli-runtime")
            .unwrap();
        let child = db.open_session(&id).unwrap();
        let lease = ExecutionLease::acquire(&self.context.paths.data_dir, child.incarnation())
            .unwrap()
            .unwrap();
        let description = "ACTIVE_AGENT_SENTINEL";
        let progress = json!({"session_id":id,"parent_session_id":parent.id(),"status":"running",
            "agent_type":"general","started_at":chrono::Utc::now().to_rfc3339()});
        db.seed_preference(&id, CHILD_PROGRESS, &progress).unwrap();
        db.seed_preference(&id, AGENT_DESCRIPTION, &json!(description))
            .unwrap();
        parent
            .write(|w| {
                w.child_invocation(
                    &child.address(),
                    &Invocation {
                        parent_session_id: parent.id().into(),
                        parent_message_id: w.append("assistant", "")?,
                        tool_use_id: format!("call-{id}"),
                        child_session_id: id.clone(),
                        attempt_id: id.clone(),
                        mode: "foreground".into(),
                        status: "starting".into(),
                        description: description.into(),
                    },
                )
            })
            .unwrap();
        (child.address(), lease)
    }
}

#[test]
fn idle_panel_keeps_foreground_sibling_after_parent_detaches() {
    let fixture = Fixture::new();
    let context = &fixture.context;
    let parent = context
        .db
        .open_session(&fixture.job.parent_session_id)
        .unwrap();
    let (child, lease) = fixture.foreground(&parent);

    // The idle tree gets no live events; both rows even have identical display labels.
    let rows = || panel_agents(context, &parent, &[]).1;
    let agents = rows();
    assert_eq!(agents.len(), 2);
    assert!(agents
        .iter()
        .all(|a| a.agent_type == "general" && a.description == "ACTIVE_AGENT_SENTINEL"));
    assert!(agents
        .iter()
        .any(|a| a.address.session == fixture.job.session_id));
    assert!(agents.iter().any(|a| a.address == child));
    drop(lease); // A lost worker must not leave a stale running row.
    let agents = rows();
    assert_eq!(agents.len(), 1);
    assert_eq!(agents[0].address.session, fixture.job.session_id);
}

#[test]
fn idle_panel_tracks_foreground_children_without_supervisor_jobs() {
    let mut fixture = Fixture::new();
    fixture.job.status = "completed".into();
    fixture.save();
    let context = &fixture.context;
    let parent = context
        .db
        .open_session(&fixture.job.parent_session_id)
        .unwrap();
    let (a, first) = fixture.foreground(&parent);
    let (b, second) = fixture.foreground(&parent);
    let addresses = |live: &[(HistoryAddress, String)]| {
        let (background, agents) = panel_agents(context, &parent, live);
        assert_eq!(background, 0);
        agents
            .into_iter()
            .map(|agent| agent.address)
            .collect::<Vec<_>>()
    };
    assert_eq!(addresses(&[]), [a.clone(), b.clone()]);
    assert_eq!(addresses(&[(a.clone(), "general".into())]), [a, b.clone()]);
    drop(first);
    assert_eq!(addresses(&[]), [b]);
    drop(second);
    assert!(addresses(&[]).is_empty());
}

#[test]
fn expired_agents_stop_counting_and_heartbeat_recovery_restores_them() {
    let mut fixture = Fixture::new();
    assert_eq!(fixture.agents().len(), 1);
    fixture.job.heartbeat_ms -= 20_000;
    fixture.save();
    assert!(fixture.agents().is_empty());
    let report = crate::extended_commands::active_agents_text(
        &fixture.context.paths,
        &fixture.job.parent_session_id,
    )
    .unwrap();
    assert!(report.starts_with("No active agents"), "{report}");
    let observed =
        background_sessions::get(&fixture.context.paths.data_dir, &fixture.job.id).unwrap();
    assert_eq!(observed.status, "unresponsive");
    assert!(observed.finished_ms.is_none());
    let saved: background_sessions::Job =
        serde_json::from_slice(&std::fs::read(&fixture.path).unwrap()).unwrap();
    assert_eq!(saved.status, "running");
    fixture.job.heartbeat_ms = chrono::Utc::now().timestamp_millis() as u64;
    fixture.save();
    assert_eq!(fixture.agents().len(), 1);
    for status in ["completed", "failed", "cancelled"] {
        fixture.job.status = status.into();
        fixture.job.finished_ms = Some(fixture.job.heartbeat_ms);
        fixture.save();
        assert!(fixture.agents().is_empty(), "{status}");
    }
}

#[test]
fn agent_and_task_durations_keep_seconds_after_an_hour_and_freeze_when_lost() {
    assert_eq!(runtime(10_000, Some(3_610_000)), "1h 0m 0s");
    assert_eq!(runtime(0, Some(64_909_000)), "18h 1m 49s");
    let mut fixture = Fixture::new();
    fixture.job.started_ms = 10_000;
    fixture.job.heartbeat_ms = 3_671_000;
    fixture.job.status = "unresponsive".into();
    assert_eq!(
        job_runtime(&fixture.job),
        "1h 1m 1s (last heartbeat; result unknown)"
    );
    fixture.job.status = "completed".into();
    fixture.job.finished_ms = Some(3_676_000);
    assert_eq!(job_runtime(&fixture.job), "1h 1m 6s");
    fixture.job.started_ms = chrono::Utc::now().timestamp_millis() as u64 - 3_661_000;
    fixture.job.heartbeat_ms = chrono::Utc::now().timestamp_millis() as u64;
    fixture.job.finished_ms = None;
    fixture.job.status = "running".into();
    fixture.save();
    let rows = agent_rows(&fixture.agents(), 90).join("\n");
    assert!(
        rows.contains("1h 1m ") && rows.contains("s  ↓ pending tokens"),
        "{rows}"
    );
}

#[test]
#[ignore = "interactive ConPTY acceptance"]
fn agent_lifecycle_terminal_fixture() {
    use crate::terminal::live::{Action, TurnDisplay};
    use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
    use std::sync::Arc;
    let fixture = Fixture::new();
    let phase = Arc::new(AtomicU8::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let update_phase = phase.clone();
    let update_stop = stop.clone();
    let mut job = fixture.job.clone();
    let path = fixture.path.clone();
    let writer = std::thread::spawn(move || {
        while !update_stop.load(Ordering::SeqCst) {
            let now = chrono::Utc::now().timestamp_millis() as u64;
            let state = update_phase.load(Ordering::SeqCst);
            job.status = match state {
                2 => "completed",
                3 => "cancelled",
                _ => "running",
            }
            .into();
            job.heartbeat_ms = if state == 1 { now - 20_000 } else { now };
            job.finished_ms = (state >= 2).then_some(now);
            std::fs::write(&path, serde_json::to_vec(&job).unwrap()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    });
    let mut display =
        TurnDisplay::new(true, "ask", &fixture._dir.path().join("keys.json"), vec![]).unwrap();
    display.set_shell_context(
        &fixture.context.paths.data_dir,
        &fixture.job.parent_session_id,
    );
    display.before_text("LIVENESS_LIVE_READY\n");
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        loop {
            match display.action().await {
                Some(Action::Submit(text)) if text == "idle" => break,
                Some(Action::Submit(text)) => {
                    phase.store(
                        match text.as_str() {
                            "stale" => 1,
                            "complete" => 2,
                            "cancel" => 3,
                            _ => 0,
                        },
                        Ordering::SeqCst,
                    );
                    display.notice(&format!("PHASE_{text}"));
                }
                Some(Action::Cancel) => break,
                _ => {}
            }
        }
    });
    display.finish(None);
    phase.store(0, Ordering::SeqCst);
    let context = fixture.context.clone();
    let parent = fixture.job.parent_session_id.clone();
    let mut editor = crate::terminal::input::editor::PromptEditor::default();
    editor.set_agent_rows(Box::new(move || {
        let rows = running_agents(&context.paths, &context.db, &parent);
        (rows.len(), rows)
    }));
    println!("LIVENESS_IDLE_READY");
    loop {
        let input = editor.readline("❯ ").unwrap();
        let text = match input {
            Some(crate::terminal::input::PromptEvent::Line(text)) => text,
            _ => break,
        };
        if text == "finish" {
            break;
        }
        phase.store(
            match text.as_str() {
                "stale" => 1,
                "complete" => 2,
                "cancel" => 3,
                _ => 0,
            },
            Ordering::SeqCst,
        );
    }
    stop.store(true, Ordering::SeqCst);
    writer.join().unwrap();
}
