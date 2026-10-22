//! SDK control messages share the active engine; stdin remains live during turns.
use crate::{
    args::Cli,
    sdk_input::{Frame, UserMessage},
    session::Session,
};
use anyhow::{bail, ensure, Context, Result};
use claude_core::native_engine::permission_response::PermissionResponse;
use serde_json::{json, Value};
enum Reply {
    Question(String),
    Permission(PermissionResponse),
}
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tokio::sync::{oneshot, Mutex};
#[path = "sdk_permission_updates.rs"]
mod permission_updates;

#[derive(Default)]
pub struct Permissions {
    pending: Mutex<HashMap<String, (Value, Option<String>, oneshot::Sender<Result<Reply>>)>>,
    cancelled: Mutex<VecDeque<String>>,
    closed: AtomicBool,
    updates: Option<Arc<permission_updates::Updates>>,
}
impl Permissions {
    pub async fn ask(
        &self,
        id: &str,
        tool_use_id: &str,
        tool: &str,
        input: &Value,
    ) -> Result<PermissionResponse> {
        match self
            .request(id, tool_use_id, tool, input, None, None)
            .await?
        {
            Reply::Permission(response) => Ok(response),
            Reply::Question(_) => bail!("Question answer cannot approve a tool"),
        }
    }

    pub async fn ask_question(
        &self,
        id: &str,
        tool_use_id: &str,
        question: &str,
        original: &Value,
    ) -> Result<String> {
        ensure!(
            original["question"] == question,
            "Active question does not match its tool input"
        );
        let options = original["options"]
            .as_array()
            .context("Question options must be an array")?;
        ensure!(
            original.get("multiSelect").is_none_or(Value::is_boolean),
            "Question multiSelect must be a boolean"
        );
        let options = options
            .iter()
            .map(|option| -> Result<Value> {
                ensure!(
                    option["label"]
                        .as_str()
                        .is_some_and(|label| !label.trim().is_empty()),
                    "Question option needs a nonempty label"
                );
                let mut option = option.clone();
                if option.get("description").is_none() {
                    option["description"] = json!("");
                }
                ensure!(
                    option["description"].is_string(),
                    "Question option description must be text"
                );
                Ok(option)
            })
            .collect::<Result<Vec<_>>>()?;
        let input = json!({"questions":[{"question":question,"header":"Question","options":options,"multiSelect":original.get("multiSelect").cloned().unwrap_or(json!(false))}]});
        match self
            .request(
                id,
                tool_use_id,
                "AskUserQuestion",
                &input,
                Some(question.to_owned()),
                original["description"].as_str(),
            )
            .await?
        {
            Reply::Question(answer) => Ok(answer),
            Reply::Permission(_) => bail!("Tool decision cannot answer a question"),
        }
    }

    async fn request(
        &self,
        id: &str,
        tool_use_id: &str,
        tool: &str,
        input: &Value,
        question: Option<String>,
        description: Option<&str>,
    ) -> Result<Reply> {
        ensure!(
            !self.closed.load(Ordering::SeqCst),
            "SDK input closed before tool approval"
        );
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .await
            .insert(id.into(), (input.clone(), question, tx));
        let mut frame = json!({"type":"control_request","request_id":id,"request":{"subtype":"can_use_tool","tool_name":tool,"input":input,"tool_use_id":tool_use_id}});
        if let Some(description) = description {
            frame["request"]["description"] = json!(description);
        }
        crate::session::emit_json(&frame)?;
        rx.await.context("SDK permission channel closed")?
    }
    pub async fn cancel_one(&self, id: &str) {
        let mut pending = self.pending.lock().await;
        if let Some((_, _, tx)) = pending.remove(id) {
            let mut cancelled = self.cancelled.lock().await;
            cancelled.push_back(id.into());
            while cancelled.len() > 256 {
                cancelled.pop_front();
            }
            let _ = tx.send(Err(anyhow::anyhow!(
                "SDK permission resolved by another source"
            )));
            let _ = crate::session::emit_json(
                &json!({"type":"control_cancel_request","request_id":id}),
            );
        }
    }
    pub async fn cancel(&self) {
        for (id, (_, _, tx)) in self.pending.lock().await.drain() {
            let _ = tx.send(Err(anyhow::anyhow!("SDK turn interrupted")));
            let _ = crate::session::emit_json(
                &json!({"type":"control_cancel_request","request_id":id}),
            );
        }
    }
    async fn respond(&self, value: &Value) -> Result<()> {
        let response = &value["response"];
        let id = response["request_id"]
            .as_str()
            .context("Missing response request_id")?;
        let Some((input, question, tx)) = self.pending.lock().await.remove(id) else {
            // The host may have sent its answer before receiving our cancel
            // frame. Its prepared policy patch must be discarded, not treated
            // as a fatal protocol error that interrupts the winning tool.
            if self
                .cancelled
                .lock()
                .await
                .iter()
                .any(|cancelled| cancelled == id)
            {
                return Ok(());
            }
            bail!("Unknown or completed SDK permission request: {id}")
        };
        let answer = (|| -> Result<Reply> {
            ensure!(
                response["subtype"] == "success",
                "SDK host rejected permission request: {}",
                response["error"]
            );
            let result = &response["response"];
            match result["behavior"].as_str() {
                Some("allow") => {
                    if let Some(question) = question.as_deref() {
                        let answer = question_answer(&input, question, &result["updatedInput"])?;
                        if let Some(updates) = result.get("updatedPermissions") {
                            self.updates
                                .as_ref()
                                .context("SDK session permission updates unavailable")?
                                .apply(updates)?;
                        }
                        Ok(Reply::Question(answer))
                    } else {
                        let updated_input = result
                            .get("updatedInput")
                            .map(|value| {
                                ensure!(value.is_object(), "SDK updatedInput must be an object");
                                Ok(value.clone())
                            })
                            .transpose()?;
                        let commit = result
                            .get("updatedPermissions")
                            .map(|value| {
                                self.updates
                                    .as_ref()
                                    .context("SDK session permission updates unavailable")?
                                    .prepare(value)
                            })
                            .transpose()?;
                        Ok(Reply::Permission(PermissionResponse::Allow {
                            updated_input,
                            commit,
                        }))
                    }
                }
                Some("deny") if question.is_some() => bail!("SDK host declined the user question"),
                Some("deny") => {
                    let message = result
                        .get("message")
                        .map(|value| value.as_str().context("SDK denial message must be text"))
                        .transpose()?
                        .unwrap_or("SDK host denied the operation")
                        .to_owned();
                    let interrupt = result
                        .get("interrupt")
                        .map(|value| {
                            value
                                .as_bool()
                                .context("SDK denial interrupt must be boolean")
                        })
                        .transpose()?
                        .unwrap_or(false);
                    Ok(Reply::Permission(PermissionResponse::Deny {
                        message,
                        interrupt,
                    }))
                }
                _ => bail!("SDK permission response needs behavior allow or deny"),
            }
        })();
        let _ = tx.send(answer);
        Ok(())
    }
}

fn question_answer(input: &Value, question: &str, updated: &Value) -> Result<String> {
    let mut fields = updated
        .as_object()
        .context("AskUserQuestion approval needs updatedInput with answers")?
        .clone();
    let answers = fields
        .remove("answers")
        .context("AskUserQuestion approval needs answers")?;
    ensure!(
        Value::Object(fields) == *input,
        "AskUserQuestion updatedInput may add answers but cannot change questions or options"
    );
    let answers = answers
        .as_object()
        .context("AskUserQuestion answers must map question text to answer strings")?;
    ensure!(
        answers.len() == 1,
        "SDK must answer exactly the requested question"
    );
    let answer = answers
        .get(question)
        .and_then(Value::as_str)
        .context("SDK answer does not match the requested question")?;
    ensure!(
        !answer.trim().is_empty() && answer.len() <= 64 * 1024,
        "SDK answer must be nonempty and at most 64 KiB"
    );
    Ok(answer.to_owned())
}

#[cfg(test)]
mod question_tests {
    use super::*;
    #[tokio::test]
    async fn approval_patch_is_deferred_until_effective_input_is_validated() {
        use claude_core::permissions::{
            AuditLogger, PermissionContext, PermissionManager, PermissionMode, PermissionResult,
        };
        let manager = Arc::new(PermissionManager::new(Arc::new(AuditLogger::new(16))));
        manager.set_mode(PermissionMode::AskPermissions);
        let updates = Arc::new(permission_updates::Updates::new(
            manager.clone(),
            "one".into(),
        ));
        let permissions = Permissions {
            updates: Some(updates),
            ..Permissions::default()
        };
        let (tx, rx) = oneshot::channel();
        permissions
            .pending
            .lock()
            .await
            .insert("request".into(), (json!({"path":"first"}), None, tx));
        permissions.respond(&json!({"response":{"subtype":"success","request_id":"request","response":{"behavior":"allow","updatedInput":{"path":"second"},"updatedPermissions":[{"type":"addRules","destination":"session","behavior":"allow","rules":[{"toolName":"Write"}]}]}}})).await.unwrap();
        assert!(matches!(
            rx.await.unwrap(),
            Ok(Reply::Permission(PermissionResponse::Allow { .. }))
        ));
        let context = PermissionContext {
            tool_name: "Write".into(),
            tool_input: json!({"path":"first"}),
            conversation_id: "one".into(),
            user_id: None,
            workspace_path: None,
        };
        assert!(matches!(
            manager.check_permission(&context),
            PermissionResult::RequiresConfirmation(_)
        ));
    }
    #[test]
    fn answer_requires_exact_question_and_unchanged_input() {
        let input =
            json!({"questions":[{"question":"Language?","multiSelect":false,"options":[]}]});
        let mut updated = input.clone();
        updated["answers"] = json!({"Language?":"自定义 Rust"});
        assert_eq!(
            question_answer(&input, "Language?", &updated).unwrap(),
            "自定义 Rust"
        );
        for answers in [
            json!({}),
            json!({"Other":"Rust"}),
            json!({"Language?":3}),
            json!({"Language?":""}),
            json!({"Language?":"Rust","Other":"Go"}),
        ] {
            updated["answers"] = answers;
            assert!(question_answer(&input, "Language?", &updated).is_err());
        }
        updated["answers"] = json!({"Language?":"Rust"});
        updated["questions"][0]["multiSelect"] = json!(true);
        assert!(question_answer(&input, "Language?", &updated).is_err());
    }
}
fn response(id: &Value, result: Result<Value>) -> Result<()> {
    crate::session::emit_json(&match result {
        Ok(value) => {
            json!({"type":"control_response","response":{"subtype":"success","request_id":id,"response":value}})
        }
        Err(error) => {
            json!({"type":"control_response","response":{"subtype":"error","request_id":id,"error":format!("{error:#}")}})
        }
    })
}
async fn idle_control(
    cli: &Cli,
    initialization: &mut crate::sdk_initialization::State,
    session: &mut Session,
    frame: &Value,
) -> Result<()> {
    let request = &frame["request"];
    let result = async {
        match request["subtype"].as_str().unwrap_or("") {
            "initialize" => initialization.initialize(cli, session, request).await,
            "interrupt" => Ok(json!({})),
            "set_permission_mode" => {
                let mode = match request["mode"].as_str().context("mode is required")? {
                    "default" => "ask",
                    "acceptEdits" => "accept-edits",
                    "bypassPermissions" => "bypass",
                    other => other,
                };
                ensure!(
                    request.get("ultraplan").is_none_or(|v| v == false),
                    "ultraplan is not supported"
                );
                session.set_permission_mode(mode).await?;
                Ok(json!({}))
            }
            "set_model" => {
                let model = request["model"]
                    .as_str()
                    .context("An explicit configured model is required")?;
                session.sdk_set_model(model).await?;
                Ok(json!({}))
            }
            other => bail!("Unsupported SDK control request: {other}"),
        }
    }
    .await;
    response(&frame["request_id"], result)
}
pub async fn run(
    cli: &Cli,
    session: &mut Session,
    initial: Option<&str>,
    source: &str,
) -> Result<()> {
    let runtime = crate::sdk_hooks::Runtime::default();
    // Declared before run_inner: dropping run first closes its event receiver,
    // then this guard revokes callbacks. The engine prioritizes receiver closure.
    let cleanup = runtime.cleanup();
    session.sdk_hooks = Some(runtime);
    session.sdk_mcp.clear();
    let transport = Arc::new(crate::sdk_mcp::Transport::default());
    let mcp_cleanup = crate::sdk_mcp::Cleanup { runtime:session.sdk_mcp.clone(), transport:transport.clone() };
    session.sdk_mcp_transport = Some(transport);
    let result = Box::pin(run_inner(cli, session, initial, source)).await;
    if result.is_err() {
        session.engine.cancel_turn(&session.id).await;
    }
    if let Some(runtime) = &session.sdk_hooks {
        runtime.broker.close();
    }
    drop(mcp_cleanup);
    session.sdk_mcp_transport.take();
    session
        .end_hooks(if result.is_ok() {
            "prompt_input_exit"
        } else {
            "other"
        })
        .await;
    session.sdk_hooks.take();
    drop(cleanup);
    if let Some(permissions) = session.sdk_permissions.take() {
        permissions.closed.store(true, Ordering::SeqCst);
        permissions.cancel().await;
    }
    session.sdk_content = None;
    session.sdk_overrides = Default::default();
    session.engine.clear_runtime_agents(&session.id);
    result
}

async fn run_inner(
    cli: &Cli,
    session: &mut Session,
    initial: Option<&str>,
    source: &str,
) -> Result<()> {
    let callbacks = session
        .sdk_hooks
        .as_ref()
        .context("SDK callback runtime unavailable")?
        .broker
        .clone();
    let mcp = session.sdk_mcp_transport.as_ref().context("SDK MCP transport unavailable")?.broker.clone();
    let updates = Arc::new(
        permission_updates::Updates::new(session.permissions.clone(), session.id.clone())
            .with_settings(permission_updates::SettingsScope {
                data_dir: session.paths.data_dir.clone(),
                workspace: session.workspace.clone(),
                project_trusted: true,
            }),
    );
    let _cleanup = permission_updates::Cleanup(updates.clone());
    let permissions = Arc::new(Permissions {
        updates: Some(updates),
        ..Permissions::default()
    });
    session.sdk_permissions = Some(permissions.clone());
    session.sdk_overrides = Default::default();
    session.engine.clear_runtime_agents(&session.id);
    let mut initialization = crate::sdk_initialization::State::default();
    let mut frames = crate::sdk_input::stdin();
    let mut queue = VecDeque::<UserMessage>::new();
    if let Some(text) = initial {
        queue.push_back(crate::sdk_input::parse_line(
            &json!({"type":"user","message":{"role":"user","content":text}}).to_string(),
        )?);
    }
    let mut eof = false;
    let mut failed = false;
    let mut started = false;
    loop {
        if !started && (initialization.is_initialized() || !queue.is_empty()) {
            initialization.start_user_turn();
            let requires_host = session
                .sdk_hooks
                .as_ref()
                .is_some_and(|runtime| runtime.has_callbacks()) || !session.sdk_mcp.names().is_empty();
            pump_lifecycle(
                Box::pin(session.start_hooks(source)),
                &callbacks,
                &mcp,
                &permissions,
                &mut frames,
                &mut queue,
                &mut eof,
                requires_host,
            )
            .await?;
            if let Some(text) = session.initial_hook_message.take() {
                let mut message = crate::sdk_input::parse_line(
                    &json!({"type":"user","message":{"role":"user","content":text}}).to_string(),
                )?;
                message.initial_hook = true;
                message.envelope["origin"] = json!({"type":"hook","hookEvent":"SessionStart"});
                queue.push_front(message);
            }
            started = true;
        }
        if let Some(message) = queue.pop_front() {
            initialization.start_user_turn();
            ensure!(
                message
                    .session_id
                    .as_deref()
                    .is_none_or(|id| id.is_empty() || id == session.id),
                "Input session_id differs from active session"
            );
            if cli.replay_user_messages {
                let mut envelope = message.envelope;
                envelope["session_id"] = json!(session.id);
                crate::session::emit_json(&envelope)?;
            }
            let mut terminal = None;
            session.sdk_content = message
                .content
                .as_array()
                .filter(|blocks| blocks.iter().any(|b| b["type"] == "image_url"))
                .map(|_| message.content.clone());
            let text = match session.sdk_overrides.initial_prompt.take() {
                Some(initial) if !initial.trim().is_empty() => {
                    if message.text.trim().is_empty() {
                        initial
                    } else {
                        format!("{initial}

{}", message.text)
                    }
                }
                _ => message.text.clone(),
            };
            let turn = Box::pin(async {
                if message.initial_hook {
                    session
                        .turn_initial_hook(cli, &text, &mut terminal, true)
                        .await
                } else {
                    session.turn(cli, &text, &mut terminal).await
                }
            });
            tokio::pin!(turn);
            let mut turn_failed = false;
            loop {
                tokio::select! {
                    result=&mut turn=>{if let Err(error)=result {turn_failed=true;failed=true;if !error.is::<crate::output::ReportedError>() {crate::session::emit_json(&json!({"type":"result","is_error":true,"session_id":session.id,"error":format!("{error:#}")}))?;}}break;},
                    frame=frames.recv(),if !eof=>match frame {
                        None=>{
                            eof=true;
                            if callbacks.has_pending() || mcp.has_pending() || session.sdk_hooks.as_ref().is_some_and(|runtime| runtime.has_callbacks()) || !session.sdk_mcp.names().is_empty() {
                                session.engine.cancel_turn(&session.id).await;
                                callbacks.close();mcp.close();session.sdk_mcp.clear();permissions.closed.store(true,Ordering::SeqCst);permissions.cancel().await;
                                failed=true;turn_failed=true;queue.clear();
                                crate::session::emit_json(&json!({"type":"result","is_error":true,"session_id":session.id,"error":"SDK input closed while a hook or MCP host was required"}))?;
                                break;
                            }
                            callbacks.close();mcp.close();permissions.closed.store(true,Ordering::SeqCst);permissions.cancel().await;
                        },
                        Some(Err(error))=>{session.engine.cancel_turn(&session.id).await;callbacks.cancel_all();mcp.cancel_all();permissions.cancel().await;return Err(error)},
                        Some(Ok(Frame::User(user)))=>{ensure!(queue.len()<32,"SDK pending user queue exceeds 32 messages");queue.push_back(user);},
                        Some(Ok(Frame::Response(value)))=>{route_response(&permissions, &callbacks, &mcp, &value).await?;},
                        Some(Ok(Frame::Control(value)))=>{
                            if value["request"]["subtype"]=="initialize" {
                                response(&value["request_id"],Err(anyhow::anyhow!("Already initialized")))?;
                                continue;
                            }
                            if value["request"]["subtype"]=="interrupt" {
                                failed=true;turn_failed=true;
                                session.engine.cancel_turn(&session.id).await;
                                callbacks.cancel_all();
                                mcp.cancel_all();
                                permissions.cancel().await;
                                response(&value["request_id"],Ok(json!({})))?;
                                crate::session::emit_json(&json!({"type":"result","is_error":true,"session_id":session.id,"error":"SDK turn interrupted"}))?;
                                break;
                            }
                            response(&value["request_id"],Err(anyhow::anyhow!("Control changes require an idle session; interrupt the active turn first")))?;
                        },
                    },
                }
            }
            mcp.cancel_all();
            permissions.cancel().await;
            if !turn_failed && session.sdk_overrides.prompt_suggestions {
                if let Err(error) = emit_prompt_suggestion(session).await {
                    if cli.verbose {
                        eprintln!("Prompt suggestion skipped: {error:#}");
                    }
                }
            }
            continue;
        }
        if eof {
            break;
        }
        match tokio::select! {frame=frames.recv()=>frame,_=tokio::signal::ctrl_c()=>bail!("SDK input interrupted")}
        {
            None => {
                eof = true;
                callbacks.close();
                mcp.close();
                session.sdk_mcp.clear();
            }
            Some(Err(error)) => return Err(error),
            Some(Ok(Frame::User(user))) => queue.push_back(user),
            Some(Ok(Frame::Control(value))) => {
                idle_control(cli, &mut initialization, session, &value).await?
            }
            Some(Ok(Frame::Response(value))) => {
                route_response(&permissions, &callbacks, &mcp, &value).await?
            }
        }
    }
    if failed {
        return Err(crate::output::ReportedError(
            "One or more SDK turns failed or were interrupted".into(),
        )
        .into());
    }
    Ok(())
}

/// Best-effort SDK prompt suggestion after a successful turn.
async fn emit_prompt_suggestion(session: &Session) -> Result<()> {
    use std::fmt::Write;
    let history = session.store.model_history(&session.id, &session.api_format)?;
    let mut transcript = String::new();
    for message in history.iter().rev().take(8).collect::<Vec<_>>().into_iter().rev() {
        let role = message["role"].as_str().unwrap_or("");
        let content = match &message["content"] {
            Value::String(text) => text.clone(),
            Value::Array(blocks) => blocks
                .iter()
                .filter_map(|block| block["text"].as_str())
                .collect::<Vec<_>>()
                .join("
"),
            _ => String::new(),
        };
        if content.trim().is_empty() {
            continue;
        }
        let _ = write!(
            transcript,
            "{role}: {}
",
            content.chars().take(2000).collect::<String>()
        );
        if transcript.len() > 12_000 {
            break;
        }
    }
    if transcript.trim().is_empty() {
        return Ok(());
    }
    let suggestion = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        session.engine.generate_text(
            &session.provider_id,
            &session.model,
            "Predict the user's likely next message in this conversation. Reply with only that message, at most 120 characters, without quotes.",
            &transcript,
        ),
    )
    .await
    .context("Prompt suggestion timed out")??;
    let suggestion = suggestion.trim().trim_matches('"').trim();
    if suggestion.is_empty() || suggestion.len() > 500 {
        return Ok(());
    }
    crate::session::emit_json(&json!({
        "type":"prompt_suggestion",
        "suggestion":suggestion,
        "uuid":uuid::Uuid::new_v4().to_string(),
        "session_id":session.id
    }))
}

async fn route_response(
    permissions: &Permissions,
    callbacks: &crate::sdk_hooks::Broker,
    mcp: &crate::sdk_control::Broker,
    value: &Value,
) -> Result<()> {
    let id = value["response"]["request_id"]
        .as_str()
        .context("Missing response request_id")?;
    if callbacks.owns(id) {
        callbacks.respond(value)
    } else if mcp.owns(id) {
        mcp.respond(value)
    } else {
        permissions.respond(value).await
    }
}

// SessionStart may invoke the SDK host. Keep processing control replies while
// its future borrows the session, and queue user turns until startup is complete.
async fn pump_lifecycle(
    future: impl std::future::Future<Output = Result<()>>,
    callbacks: &crate::sdk_hooks::Broker,
    mcp: &crate::sdk_control::Broker,
    permissions: &Permissions,
    frames: &mut tokio::sync::mpsc::Receiver<Result<Frame>>,
    queue: &mut VecDeque<UserMessage>,
    eof: &mut bool,
    requires_host: bool,
) -> Result<()> {
    tokio::pin!(future);
    loop {
        tokio::select! {
            result = &mut future => return result,
            _ = tokio::signal::ctrl_c() => bail!("SDK startup interrupted"),
            frame = frames.recv(), if !*eof => match frame {
                None => {
                    *eof = true;
                    let interrupted = requires_host && !queue.is_empty();
                    callbacks.close();mcp.close();permissions.closed.store(true,Ordering::SeqCst);permissions.cancel().await;
                    ensure!(!interrupted,"SDK input closed during startup hook callback");
                },
                Some(Err(error)) => return Err(error),
                Some(Ok(Frame::Response(value))) => route_response(permissions,callbacks,mcp,&value).await?,
                Some(Ok(Frame::User(user))) => { ensure!(queue.len()<32,"SDK pending user queue exceeds 32 messages");queue.push_back(user); },
                Some(Ok(Frame::Control(value))) => {
                    if value["request"]["subtype"]=="interrupt" {
                        callbacks.cancel_all();
                        mcp.cancel_all();
                        response(&value["request_id"],Ok(json!({})))?;
                        bail!("SDK startup interrupted");
                    }
                    // Startup only begins after initialization (or the first user
                    // turn), so a repeated initialize gets the same answer as mid-turn.
                    if value["request"]["subtype"]=="initialize" {
                        response(&value["request_id"],Err(anyhow::anyhow!("Already initialized")))?;
                        continue;
                    }
                    response(&value["request_id"],Err(anyhow::anyhow!("Control changes require completed session startup")))?;
                }
            }
        }
    }
}
