//! Explicit worker-owned child wake API. No discovery of unrelated sessions or providers.
use super::{EngineEvent, QueryEngine, ToolLoopExecutor};
use crate::services::{agents, session_messages, EnvironmentCredentials, ServiceContext};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::sync::atomic::Ordering;

impl QueryEngine {
    pub fn owned_child_permission_target(
        &self,
        owner: &str,
        id: &str,
    ) -> Result<(
        std::sync::Arc<crate::permissions::PermissionManager>,
        crate::native_engine::provider_manager::ResolvedProvider,
    )> {
        let children = self
            .owned_children
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let child = children
            .get(id)
            .context("Original owned child runtime unavailable")?;
        ensure!(
            child.parent == owner && child.idle_ready,
            "Child is not eligible for this owner"
        );
        child.permission_target()
    }

    pub fn pause_owned_child_idle(&self, owner: &str, id: &str) {
        if let Some(child) = self
            .owned_children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(id)
        {
            if child.parent == owner {
                child.idle_ready = false;
            }
        }
    }

    /// Only a successful explicit worker turn enables autonomous inbox continuations.
    pub fn set_owned_child_idle_ready(&self, owner: &str, ready: bool) {
        for child in self
            .owned_children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values_mut()
        {
            if child.parent == owner {
                child.idle_ready = ready;
            }
        }
    }

    /// Inspect only runtimes already retained by this engine for this exact owner.
    pub async fn pending_owned_child(&self, owner: &str) -> Result<Option<String>> {
        if self.active_turns.lock().await.contains_key(owner) {
            return Ok(None);
        }
        let ids = self
            .owned_children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter(|(_, child)| {
                child.parent == owner
                    && child.idle_ready
                    && child.owner_budget.load(Ordering::Acquire) < 8
            })
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        if ids.is_empty() {
            return Ok(None);
        }
        let root = self
            .provider_manager
            .lock()
            .await
            .config_path()
            .parent()
            .context("Runtime directory missing")?
            .to_path_buf();
        let context = ServiceContext {
            paths: crate::runtime::RuntimePaths::new(root.clone())?,
            db: self.db_manager.clone(),
            credentials: std::sync::Arc::new(EnvironmentCredentials),
        };
        if session_messages::receiver_active(&root, owner)? {
            return Ok(None);
        }
        for id in ids {
            if session_messages::pending_count(&context, &id)? > 0 {
                return Ok(Some(id));
            }
        }
        Ok(None)
    }

    /// Run a single retained child under its original credentials; the parent makes no request.
    /// The caller must own a visible worker job and route approval/cancellation events.
    pub async fn wake_owned_child(
        &self,
        owner: &str,
        child: &str,
        events: tokio::sync::mpsc::Sender<EngineEvent>,
    ) -> Result<Value> {
        ensure!(
            self.owned_children
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(child)
                .is_some_and(|entry| entry.parent == owner && entry.idle_ready),
            "Original owned child runtime is unavailable; needs_resume"
        );
        ensure!(
            !self.active_turns.lock().await.contains_key(owner),
            "Owner has an active turn"
        );
        let root = self
            .provider_manager
            .lock()
            .await
            .config_path()
            .parent()
            .context("Runtime directory missing")?
            .to_path_buf();
        let context = ServiceContext {
            paths: crate::runtime::RuntimePaths::new(root.clone())?,
            db: self.db_manager.clone(),
            credentials: std::sync::Arc::new(EnvironmentCredentials),
        };
        let _owner_lease = session_messages::ExecutionLease::acquire(&root, owner)?;
        let outcome: Result<Value> = async {
            ensure!(
                session_messages::pending_count(&context, child)? > 0,
                "Child inbox is empty"
            );
            let (_, project) = self.db_manager.with_conn(|conn| {
                crate::native_engine::workspace::resolve_workspace(
                    conn,
                    &self.workspaces_dir,
                    owner,
                    None,
                )
            })??;
            let agent = agents::for_session(&self.db_manager, owner)?;
            let tools = agent.as_ref().and_then(|agent| agent.tools.clone());
            let read_only_shell = agent.as_ref().is_some_and(|agent| agent.read_only_shell);
            let directories = project
                .map(|project| {
                    project
                        .folders
                        .into_iter()
                        .map(|folder| folder.path.into())
                        .collect()
                })
                .unwrap_or_default();
            let host = ToolLoopExecutor::idle_owner_for_child(
                self.owned_children.clone(),
                owner,
                child,
                events,
            )?
            .with_allowed_tools(tools)
            .with_read_only_shell(read_only_shell)
            .with_allowed_directories(directories)
            .with_permission_manager(self.permission_manager.clone())
            .with_answer_waiters(self.answer_waiters.clone())
            .with_permission_mailbox(self.permission_mailbox.clone());
            let mut result = Box::pin(host.resume_owned_child(child, None, true)).await?;
            result["resumed_in_owner_turn"] = json!(false);
            result["trigger"] = json!("idle_inbox");
            let text = result["result"].to_string();
            let mut boundary = text.len().min(60 * 1024);
            while !text.is_char_boundary(boundary) {
                boundary -= 1;
            }
            if let Err(error) = session_messages::send(
                &context,
                child,
                owner,
                &format!(
                    "Owned child {child} completed an idle inbox task: {}",
                    &text[..boundary]
                ),
            ) {
                result["delivery_error"] = json!(error.to_string());
            }
            if let Some(entry) = self
                .owned_children
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get_mut(child)
            {
                entry.idle_ready = true;
            }
            Ok(result)
        }
        .await;
        if let Err(error) = &outcome {
            // Leave queued inbox data intact. An invalid runtime is never retried every tick.
            self.owned_children
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(child);
            let key = format!("delegation.{child}");
            if let Some(mut saved) = context.setting::<Value>(&key)? {
                if saved["parent_session_id"] == owner && saved["status"] == "completed" {
                    saved["status"] = json!("needs_resume");
                    saved["error"] = json!(error.to_string());
                    context.save_setting(&key, &saved)?;
                }
            }
        }
        outcome
    }
}
