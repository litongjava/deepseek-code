//! The plaintext authority is derived from a live host decision or trusted storage,
//! never from tool arguments, a generic tool approval, or a permission mode.
use super::*;
use crate::native_engine::permission_response::{self, Pending, PermissionResponse};
use crate::permissions::http_grants::{self, GrantScope};
use std::path::Path;

impl ToolLoopExecutor {
    fn http_store(&self) -> Option<(&Path, &Path, &str)> {
        let data = crate::data_layout::data_root_of(self.search_config_path.as_deref()?)?;
        let session = self
            .file_state
            .session
            .as_ref()
            .map(|s| s.incarnation())
            .unwrap_or("");
        Some((data, Path::new(&self.workspace_cwd), session))
    }

    pub(super) fn http_grant(&self, origin: &str) -> Result<Option<GrantScope>> {
        match self.http_store() {
            Some((data, project, session)) => http_grants::lookup(data, project, session, origin),
            None => Ok(None),
        }
    }

    pub(super) async fn run_http_fetch(&self, input: &Value, id: &str) -> Result<Value> {
        let url = reqwest::Url::parse(input["url"].as_str().context("Missing WebFetch URL")?)?;
        anyhow::ensure!(
            url.scheme() == "http",
            "Plaintext approval requires an HTTP URL"
        );
        let origin = url.origin().ascii_serialization();
        if let Some(reason) = self.permission_hard_denial("WebFetch", input) {
            anyhow::bail!("{reason}");
        }
        let local = crate::permissions::web_fetch::requires_local_approval(url.as_str());
        let saved = self.http_grant(&origin)?.is_some();
        let reason = if !local && !saved {
            match self.fetch_http_attempt(input, None).await {
                Err(error) if error.is::<crate::tools::HttpFallbackRequired>() => {
                    Some(error.to_string())
                }
                result => return result,
            }
        } else {
            None
        };
        crate::tools::validate_plaintext_http_input(input)?;
        let explicit_local_ask = local
            && self.permission_manager.as_ref().is_some_and(|pm| {
                let decision = pm.evaluate_permission(&crate::permissions::PermissionContext {
                    tool_name: "WebFetch".into(),
                    tool_input: input.clone(),
                    conversation_id: self.conv_id.clone().unwrap_or_default(),
                    workspace_path: Some(self.workspace_cwd.clone()),
                    mcp_read_only: false,
                    user_id: None,
                });
                matches!(decision.result, PermissionResult::RequiresConfirmation(_))
                    && matches!(
                        decision.origin,
                        crate::permissions::DecisionOrigin::ExplicitRule
                            | crate::permissions::DecisionOrigin::ToolSetting
                    )
            });
        let exact = if saved && !explicit_local_ask {
            false
        } else {
            let mut review = json!({"url":url.as_str(), "origin":origin, "local":local});
            if let Some(reason) = reason {
                review["reason"] = json!(reason);
            }
            let scope = self.ask_http_scope(id, review).await?;
            if let Some(reason) = self.permission_hard_denial("WebFetch", input) {
                anyhow::bail!("{reason}");
            }
            if let Some(scope) = scope {
                let (data, project, session) = self
                    .http_store()
                    .context("WebFetch HTTP grant storage unavailable")?;
                http_grants::grant(data, project, session, &origin, scope)?;
                false
            } else {
                true
            }
        };
        self.fetch_http_attempt(
            input,
            Some((if exact { url.as_str() } else { &origin }, exact)),
        )
        .await
    }

    async fn ask_http_scope(&self, id: &str, review: Value) -> Result<Option<GrantScope>> {
        anyhow::ensure!(
            self.permission_host_available && self.toolscript_approval.is_none(),
            "Plaintext WebFetch denied: no approval host is available for this request"
        );
        anyhow::ensure!(
            !self
                .permission_manager
                .as_ref()
                .is_some_and(|pm| pm.get_mode() == crate::permissions::PermissionMode::DontAsk),
            "Plaintext WebFetch denied in dontAsk mode"
        );
        let request_id = uuid::Uuid::new_v4().to_string();
        let reply = permission_response::register(&self.permission_mailbox, &request_id).await;
        let _pending = Pending {
            mailbox: self.permission_mailbox.clone(),
            request_id: request_id.clone(),
        };
        let _owner = PermissionWaiterGuard {
            waiters: self.answer_waiters.clone(),
            request_id: request_id.clone(),
        };
        permission_response::register_owner(
            &request_id,
            self.conv_id.as_deref().unwrap_or_default(),
            id,
            "permission",
            review.clone(),
            None,
            None,
            None,
        );
        permission_response::set_owner_tool(&request_id, "WebFetchHttp");
        self.event_tx
            .send(EngineEvent::ToolPermission {
                request_id,
                tool_use_id: id.into(),
                tool_name: "WebFetchHttp".into(),
                input: review,
            })
            .await
            .map_err(|_| anyhow!("Approval host disconnected"))?;
        let wait = async {
            tokio::select! {
                answer = reply => answer.context("Plaintext approval cancelled"),
                _ = self.event_tx.closed() => Err(anyhow!("Approval host disconnected")),
            }
        };
        let answer = if self.durable_user_input {
            wait.await?
        } else {
            timeout(Duration::from_secs(300), wait)
                .await
                .context("Plaintext approval timed out")??
        };
        match answer {
            PermissionResponse::Text(choice) => match choice.as_str() {
                "http_once" => Ok(None),
                "http_session" => Ok(Some(GrantScope::Session)),
                "http_project" => Ok(Some(GrantScope::Project)),
                "http_user" => Ok(Some(GrantScope::User)),
                _ => anyhow::bail!("Plaintext WebFetch denied"),
            },
            PermissionResponse::Deny { message, interrupt } => {
                if interrupt {
                    *self
                        .permission_interrupted
                        .lock()
                        .unwrap_or_else(|e| e.into_inner()) = Some(message.clone());
                }
                anyhow::bail!("{message}");
            }
            _ => anyhow::bail!("Plaintext WebFetch requires an explicit HTTP scope"),
        }
    }
}
