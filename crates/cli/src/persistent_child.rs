//! Supervised idle jobs for owned children and internal command-hook feedback.
use super::{jobs, service, ModeControl, ModeTarget, Session, Worker};
use anyhow::{bail, Context, Result};
use claude_core::native_engine::tool_loop::EngineEvent;
use serde_json::json;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

pub(super) async fn start_pending(
    worker: &Worker,
    sessions: &Arc<tokio::sync::Mutex<Option<Session>>>,
    active: &Arc<AtomicBool>,
    idle_active: &Arc<AtomicBool>,
    fatal: &Arc<AtomicBool>,
    mode_target: &ModeTarget,
) -> Result<()> {
    let Ok(current) = sessions.try_lock() else {
        return Ok(());
    };
    let Some(session) = current.as_ref() else {
        return Ok(());
    };
    let child =
        if claude_core::services::async_hooks::has_pending_wake(&worker.root, &worker.session_id) {
            String::new()
        } else if let Some(child) = session
            .engine
            .pending_owned_child(&worker.session_id)
            .await?
        {
            child
        } else {
            return Ok(());
        };
    if active
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Ok(());
    }
    let id = uuid::Uuid::new_v4().to_string();
    if let Err(error) = service::begin_job(
        worker,
        &id,
        &if child.is_empty() {
            "Async hook blocking feedback".into()
        } else {
            format!("Resume owned child {child} from inbox")
        },
    ) {
        active.store(false, Ordering::Release);
        return Err(error);
    }
    // Automatic work is visible under /tasks and never seizes the input prompt.
    let publish = (|| -> Result<()> {
        if !child.is_empty() {
            std::fs::write(
                worker
                    .root
                    .join("background-tasks")
                    .join(&id)
                    .join("owned-child.json"),
                serde_json::to_vec(&child)?,
            )?;
        }
        jobs::detach(&worker.root, &id, &worker.session_id)
    })();
    if let Err(error) = publish {
        let _ = service::finish_job(worker, &id, Some(error.to_string())).await;
        active.store(false, Ordering::Release);
        return Err(error);
    }
    idle_active.store(true, Ordering::Release);
    drop(current);
    let worker = worker.clone();
    let sessions = sessions.clone();
    let active = active.clone();
    let idle_active = idle_active.clone();
    let fatal = fatal.clone();
    let mode_target = mode_target.clone();
    tokio::spawn(async move {
        let task_worker = worker.clone();
        let task_id = id.clone();
        let target = mode_target.clone();
        let cleanup_sessions = sessions.clone();
        let cleanup_child = child.clone();
        let result = tokio::spawn(async move {
            Box::pin(execute(&task_worker, &task_id, &child, sessions, target)).await
        })
        .await;
        let error = match result {
            Ok(result) => result.err().map(|error| format!("{error:#}")),
            Err(error) => {
                fatal.store(true, Ordering::Release);
                Some(format!("Owned child worker panicked: {error}"))
            }
        };
        if error.is_some() && !cleanup_child.is_empty() {
            if let Some(session) = cleanup_sessions.lock().await.as_ref() {
                session
                    .engine
                    .pause_owned_child_idle(&session.id, &cleanup_child);
            }
        }
        if let Some(error) = &error {
            let _ = service::emit_event(&json!({"type":"error","error":error}));
        }
        if service::finish_job(&worker, &id, error).await.is_err() {
            fatal.store(true, Ordering::Release);
        }
        *mode_target.lock().unwrap_or_else(|e| e.into_inner()) = None;
        service::set_event_sink(None);
        std::env::remove_var(jobs::CONTROL_ENV);
        idle_active.store(false, Ordering::Release);
        active.store(false, Ordering::Release);
    });
    Ok(())
}

async fn execute(
    worker: &Worker,
    id: &str,
    child: &str,
    sessions: Arc<tokio::sync::Mutex<Option<Session>>>,
    mode_target: ModeTarget,
) -> Result<()> {
    let dir = worker.root.join("background-tasks").join(id);
    service::set_event_sink(Some(dir.join("output.jsonl")));
    std::env::set_var(jobs::CONTROL_ENV, &dir);
    let current = sessions.lock().await;
    let session = current
        .as_ref()
        .context("Owner runtime is unavailable; needs_resume")?;
    session
        .permissions
        .set_session_rules(&session.id, crate::permission_commands::load(session)?);
    if let Some(mode) =
        claude_core::services::session_permission_mode::load(&session.store.db, &session.id)?
    {
        if mode != session.permissions.get_mode() {
            session.permissions.set_mode(mode);
        }
    }
    if child.is_empty() {
        let cli = session
            .hook_wake_cli
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .context("Hook wake requires live worker options")?;
        *mode_target.lock().unwrap_or_else(|e| e.into_inner()) = Some(ModeControl {
            job_id: id.into(),
            session_id: session.id.clone(),
            manager: session.permissions.clone(),
            db: session.store.db.clone(),
            provider: session.selected_provider().await?,
            owner_manager: None,
            bypass_available: session.bypass_available,
            full_access: session.full_access,
        });
        return session.wake_hooks(&cli, &mut None).await;
    }
    let (manager, provider) = session
        .engine
        .owned_child_permission_target(&session.id, child)?;
    *mode_target.lock().unwrap_or_else(|e| e.into_inner()) = Some(ModeControl {
        job_id: id.into(),
        session_id: child.into(),
        manager,
        db: session.store.db.clone(),
        provider,
        owner_manager: Some(session.permissions.clone()),
        // A child never gains bypass of its own.
        bypass_available: false,
        full_access: false,
    });
    service::emit_event(
        &json!({"type":"owned_child_wake","session_id":session.id,"child_session_id":child}),
    )?;
    let (events, mut receiver) = tokio::sync::mpsc::channel(500);
    let mut execution = Box::pin(session.engine.wake_owned_child(&session.id, child, events));
    type Answer<'a> = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Option<(String, String, bool)>>> + Send + 'a>,
    >;
    let mut pending_answer: Option<Answer<'_>> = None;
    loop {
        tokio::select! {
            biased;
            _ = jobs::wait_for_cancellation() => bail!("Owned child {child} task cancelled"),
            answer = async { match pending_answer.as_mut() { Some(answer) => answer.await, None => std::future::pending().await } } => {
                pending_answer = None;
                if let Some((request_id, answer, permission)) = answer? {
                    let delivered = session.engine.resume_with_answer(&request_id, answer).await;
                    // A hook may atomically win after the host answer was read.
                    if !permission { delivered?; }
                }
            },
            Some(event) = receiver.recv(), if pending_answer.is_none() => {
                if let Some(mut value) = crate::session::event_json(&event) {
                    value["child_session_id"] = json!(child);
                    service::emit_event(&value)?;
                }
                match event {
                    EngineEvent::ToolPermission { request_id, tool_name, input, .. } => {
                        let mut closed = session.engine.permission_completion(&request_id).await;
                        if *closed.borrow() { continue; }
                        let question = format!("Owned child {child}: allow {tool_name} {}? [y/N]", serde_json::to_string(&input)?);
                        pending_answer = Some(Box::pin(async move {
                            tokio::select! {
                                biased;
                                _ = closed.wait_for(|closed| *closed) => Ok(None),
                                answer = jobs::worker_answer(&worker.root, &session.id, &request_id, "permission", &question) => {
                                    Ok(Some((request_id.clone(), answer?.context("Worker approval channel missing")?, true)))
                                }
                            }
                        }));
                    },
                    EngineEvent::AskUser { request_id, question, options, tool_input, .. } => {
                        let question = format!("Owned child {child}: {question}\n{}", options.iter().enumerate().map(|(i, value)|format!("{}. {value}",i+1)).collect::<Vec<_>>().join("\n"));
                        pending_answer = Some(Box::pin(async move {
                            let answer = jobs::worker_answer(&worker.root, &session.id, &request_id, "question", &question).await?.context("Worker answer channel missing")?;
                            let answer = answer.trim().parse::<usize>().ok().and_then(|i| i.checked_sub(1)).and_then(|i| options.get(i)).cloned().unwrap_or(answer);
                            let answer = if tool_input["subtype"] == "elicitation" {
                                claude_core::mcp::elicitation::user_response(&answer)
                                    .unwrap_or_else(|_| json!({"action":"cancel"})).to_string()
                            } else { answer };
                            Ok(Some((request_id, answer, false)))
                        }));
                    },
                    _ => {},
                }
            },
            result = &mut execution => {
                let result = result?;
                service::emit_event(&json!({"type":"result","session_id":session.id,"child_session_id":child,"is_error":false,"result":result}))?;
                return Ok(());
            },
        }
    }
}
