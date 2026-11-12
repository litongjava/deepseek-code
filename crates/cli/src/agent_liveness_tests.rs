use super::*;
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
            .join("background-tasks")
            .join(&id)
            .join("state.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        context
            .save_setting(
                &format!("background-agent.{id}"),
                &json!({"agent_type":"general","description":"ACTIVE_AGENT_SENTINEL"}),
            )
            .unwrap();
        context.save_setting(&format!("delegation.{child}"), &json!({"parent_session_id":parent,"status":"running","description":"ACTIVE_AGENT_SENTINEL"})).unwrap();
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
        running_agents(&context.paths, &context.db, &parent)
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
