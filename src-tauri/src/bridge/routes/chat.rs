use deepseek_code_core::db::session_store::{SegmentPlacement,SegmentUpdate};
use deepseek_code_core::{services::input_queue::QueueItem,native_engine::provider_manager::ApiFormat};
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::sse::{Event, KeepAlive, Sse},
    routing::{get, post},
    Json, Router,
};
use axum::response::IntoResponse;
use serde_json;
use std::convert::Infallible;
use tokio::sync::broadcast;
use tokio::time::Duration;

use super::super::{AppState, StreamQuery, set_sse_content_type};
use crate::native_engine::NativeEngine;
use crate::native_engine::engine_core::ChatRequest;
use crate::streaming::{StreamManager, SSE_IDLE_TIMEOUT_SECS, SSE_MAX_DURATION_SECS};
use crate::config::ConfigManager;

pub(super) async fn chat_handler(
    State(state): State<AppState>,
    Json(req): Json<ChatRequest>,
) -> impl IntoResponse {
    let _gate = super::input_queue::gate(&state, &req.conversation_id).await;
    if state.generation_runs.is_active(&req.conversation_id) {
        return (StatusCode::CONFLICT, Json(serde_json::json!({"error":"Conversation is running; enqueue input instead"}))).into_response();
    }
    start_chat(state, req, None).await
}

pub(super) async fn start_chat(
    state: AppState,
    req: ChatRequest,
    queued: Option<(QueueItem,ApiFormat)>,
) -> axum::response::Response {
    start_authorized_chat(state,req,queued,None).await
}

pub(super) async fn start_authorized_chat(
    state: AppState,
    req: ChatRequest,
    queued: Option<(QueueItem,ApiFormat)>,
    grant: Option<deepseek_code_core::db::session_store::H5Session>,
) -> axum::response::Response {
    let h5 = grant.is_some();
    crate::metrics::HTTP_REQUESTS_TOTAL.inc();
    let _request_timer = crate::metrics::HTTP_REQUEST_DURATION.start_timer();

    let id = req.conversation_id.clone();
    let span = tracing::info_span!("chat_stream", conversation_id = %id);
    let _ = span;

    {
        let rate_limiter = &state.rate_limiter;
        if !rate_limiter.check("chat_handler").await {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [("X-RateLimit-Remaining", "0"), ("Retry-After", "60")],
                Json(serde_json::json!({"error": "Rate limit exceeded"})),
            ).into_response();
        }
    }

    tracing::info!(module = "Chat", "Handler ENTRY POINT");

    // Reject invalid Code/project requests before persisting messages or invoking a model.
    let db = state.db_manager.clone();
    let project_conversation_id = req.conversation_id.clone();
    match tokio::task::spawn_blocking(move ||db.validate_chat_project(&project_conversation_id)).await {
        Ok(Ok(()))=>{},
        Ok(Err(error))=>return (StatusCode::BAD_REQUEST,Json(serde_json::json!({"error":error.to_string()}))).into_response(),
        Err(_)=>return (StatusCode::INTERNAL_SERVER_ERROR,Json(serde_json::json!({"error":"Could not validate conversation project"}))).into_response(),
    }

    let user_content = req
        .messages
        .last()
        .filter(|m| m.get("role").and_then(|v| v.as_str()) == Some("user"))
        .and_then(|m| m.get("content"))
        .cloned();
    if let Some(content) = &user_content {
        if !content.is_string() {
            if let Err(error) = crate::runtime::normalize_user_content(content) {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error":error.to_string()})),
                )
                    .into_response();
            }
        }
    }
    let user_content = if queued.is_some() { None } else { user_content };
    let (execution_lease, sealed_first) = match state.db_manager.admit_chat_execution(
        &req.conversation_id,
        &req.model,
        user_content.as_ref(),
        "typed",
    ) {
        Ok((lease, sealed)) => (std::sync::Arc::new(lease), sealed),
        Err(error) => {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({"error":error.to_string()})),
            )
                .into_response()
        }
    };
    let native_engine = state.native_engine.clone();
    let config_manager = state.config_manager.clone();
    let conv_id = req.conversation_id.clone();
    let model = req.model.clone();
    let queue_context = (!h5).then(|| {
        let mut context = req.clone();
        context.messages.clear();
        context
    });
    let lifecycle_state = state.clone();
    let lifecycle_id = conv_id.clone();
    let requested_count = req.messages.len();

    tracing::info!(
        module = "Chat",
        "[1/7] Parsed request: conv_id={}, model={}, messages={}",
        conv_id,
        model,
        requested_count
    );

    // Save user messages to SQLite before starting the engine
    tracing::info!(module = "Chat", "[2/7] Starting SQLite save...");
    if !sealed_first {
        let db = state.db_manager.clone();
        let conv_id_save = conv_id.clone();
        let model_save = model.clone();
        let saved = tokio::task::spawn_blocking(move || {
            db.admit_chat_message(&conv_id_save, &model_save, user_content.as_ref(), "typed")
        })
        .await;
        if !matches!(saved, Ok(Ok(()))) {
            tracing::error!("Could not persist user content: {:?}", saved);
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error":"Could not save user message"}))).into_response();
        }
    }
    if let Some(context) = queue_context {
        if let Err(error) = state.db_manager.remember_desktop_context(&context) {
            return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error":error.to_string()}))).into_response();
        }
    }
    tracing::info!(module = "Chat", "[2/7] SQLite save COMPLETED");

    let timer = match &queued {
        Some((item,_)) => state.db_manager.start_desktop_input(&conv_id,execution_lease.clone(),item),
        None => state.db_manager.start_turn_timer(&conv_id),
    };
    let mut turn =
        match timer {
            Ok(turn) => turn,
            Err(error)
                if error
                    .downcast_ref::<deepseek_code_core::services::input_queue::QueueError>()
                    .is_some() =>
            {
                return (
                    StatusCode::CONFLICT,
                    Json(serde_json::json!({"error":error.to_string()})),
                )
                    .into_response()
            }
            Err(error) => return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(
                    serde_json::json!({"error":format!("Could not start turn timing: {error:#}")}),
                ),
            )
                .into_response(),
        };
    let first_assistant_id = match turn.ensure_segment() {
        Ok(id) => id,
        Err(error) => return (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error":format!("Could not persist assistant segment: {error:#}")}))).into_response(),
    };
    let run = state.generation_runs.start(&conv_id);
    run.set_turn(turn.record(), &first_assistant_id);

    // Sync providers from ConfigManager to NativeEngine before each request
    tracing::info!(module = "Chat", "[3/7] Syncing providers...");
    let providers_to_sync = {
        let cm_guard: tokio::sync::MutexGuard<'_, Option<ConfigManager>> = config_manager.lock().await;
        if let Some(cm) = cm_guard.as_ref() {
            cm.get_config().providers.iter().map(crate::runtime::provider_from_config).collect::<Vec<_>>()
        } else {
            Vec::new()
        }
    };
    tracing::info!(module = "Chat", "[3/7] Providers synced: count={}", providers_to_sync.len());

    tracing::info!(module = "Chat", "[4/7] Locking native_engine...");
    let mut start_error = "Native engine is not initialized".to_string();
    let rx_opt = {
        let mut engine_guard: tokio::sync::MutexGuard<'_, Option<NativeEngine>> = native_engine.lock().await;
        tracing::info!(module = "Chat", "[4/7] NativeEngine lock acquired");

        if let Some(engine) = engine_guard.as_mut() {
            tracing::info!(module = "Chat", "[5/7] Engine exists, syncing providers and setting permission mode...");

            // Sync latest providers
            engine.sync_providers(providers_to_sync).await;

            let model = if model.is_empty() {
                "claude-sonnet-4-6"
            } else {
                &model
            };
            let format = match queued.as_ref() {
                Some((_, format)) => format.clone(),
                None => {
                    let provider = match req.provider_id.as_deref() {
                        Some(id) => engine.resolved_provider(id, model).await.ok(),
                        None => engine.resolve_provider(model).await,
                    };
                    let Some(provider) = provider else {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(serde_json::json!({"error":"Chat model unavailable"})),
                        )
                            .into_response();
                    };
                    provider.provider.api_format
                }
            };
            let (messages, context_boundary) =
                match crate::runtime::SessionStore::new(state.db_manager.clone())
                    .model_history_with_boundary(&conv_id, &format)
                {
                    Ok(history) => history,
                    Err(error) => {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(serde_json::json!({"error":error.to_string()})),
                        )
                            .into_response()
                    }
                };

            let chat_req = crate::native_engine::engine_core::ChatRequest {
                max_turns: None,
                response_schema: None,
                provider_id: req.provider_id.clone(), effort: req.effort.clone(),
                conversation_id: conv_id.clone(),
                context_boundary,
                assistant_message_id: Some(first_assistant_id.clone()),
                messages,
                model: model.to_owned(),
                system_prompt: req.system_prompt.clone(),
                standalone_frontend: false,
                max_tokens: req.max_tokens,
                workspace_path: req.workspace_path.clone(),
                temperature: req.temperature,
                top_p: req.top_p,
                reasoning_mode: req.reasoning_mode.clone(),
            };

            tracing::info!(module = "Chat", "[6/7] Calling engine.send_message...");

            let result = tokio::select! {
                result = engine.send_message_with_lease(chat_req, false, execution_lease.clone()) => result,
                _ = run.stop_requested() => Err(anyhow::anyhow!("Turn cancelled before model request")),
            };
            match result {
                Ok(rx) => {
                    tracing::info!(module = "Chat", "[6/7] send_message SUCCESS, got event receiver");
                    Some(rx)
                }
                Err(e) => {
                    let err_msg = format!("{}", e);
                    tracing::error!(module = "Chat", "NativeEngine send_message error: {}", err_msg);
                    start_error = err_msg;
                    None
                }
            }
        } else {
            tracing::error!(module = "Chat", "NativeEngine not initialized");
            None
        }
    };
    tracing::info!(module = "Chat", "[7/7] Engine guard released, building SSE stream...");
    // Registered while this collector owns the receiver; stop-generation signals it.
    let started = rx_opt;

    let (sse_tx, sse_rx) = tokio::sync::mpsc::channel(64);
    // Execution belongs to the backend; disconnecting an observer does not stop it.
    let db_for_stream = state.db_manager.clone();
    crate::metrics::ACTIVE_SSE_CONNECTIONS.inc();
    let stream = async_stream::stream! {
        // Keep the session owned until this stream has persisted the turn.
        let _h5_grant = grant;
        let _turn_lease = execution_lease;
        let db_clone = db_for_stream;
        yield Ok::<Event, Infallible>(Event::default().data(serde_json::json!({"type":"turn_timing","turn":turn.record(),"assistant_id":first_assistant_id}).to_string()));
        let mut rx = match started {
            Some(rx) => rx,
            None => {
                tracing::error!(module = "Chat", "rx_opt is None - engine failed to start");
                let status = if start_error == "Turn cancelled before model request" { deepseek_code_core::db::turn_repo::TurnStatus::Cancelled } else { deepseek_code_core::db::turn_repo::TurnStatus::Failed };
                match turn.finish(status) {
                    Ok(record) => yield Ok::<Event, Infallible>(Event::default().data(serde_json::json!({"type":"turn_timing","turn":record,"assistant_id":first_assistant_id}).to_string())),
                    Err(error) => tracing::error!("Could not finalize failed turn: {error:#}"),
                }
                let _ = deepseek_code_core::services::input_queue::set_paused(&db_clone, &conv_id, true);
                drop(run);
                crate::metrics::ACTIVE_SSE_CONNECTIONS.dec();
                yield Ok::<Event, Infallible>(Event::default().data(serde_json::json!({"type": "error", "error": start_error}).to_string()));
                return;
            }
        };

        let history=turn.history();
        let mut assistant_id = first_assistant_id;
        let mut observed_plan_boundary = history.latest_plan_boundary().unwrap_or(None);
        let mut request_snapshots = Vec::<serde_json::Value>::new();
        let numbers=history.clone();
        let mut request_number = tokio::task::spawn_blocking(move || numbers.last_request_number())
            .await.ok().and_then(Result::ok).unwrap_or(0);
        let terminal_event;
        let mut permission_watches = super::permission_watch::PermissionWatches::default();
        let mut full_text = String::new();
        let mut tools = Vec::new();
        let mut origins = deepseek_code_core::services::tool_history::ToolOrigins::new();
        let mut request_links = deepseek_code_core::services::tool_history::ToolRequestLinks::new();
        let mut arguments = std::collections::HashMap::new();

        loop {
            let next = tokio::select! {
                biased;
                _ = run.stop_requested() => {
                    // Text the engine already produced is part of the partial answer,
                    // even if this loop had not consumed it when Stop arrived.
                    while let Ok(pending) = rx.try_recv() {
                        if let crate::native_engine::tool_loop::EngineEvent::Text(text) = pending {
                            full_text.push_str(&text);
                        }
                    }
                    // Same path as a disconnect: dropping rx below cancels this turn's executor.
                    terminal_event = Some(serde_json::json!({"type":"message_stop","stop_reason":"cancelled","full_text":full_text}));
                    break;
                }
                resolved = permission_watches.next(&conv_id), if !permission_watches.is_empty() => {
                    yield Ok::<Event, Infallible>(Event::default().data(resolved.to_string()));
                    continue;
                }
                event = rx.recv() => event,
            };
            let event = match next {
                Some(e) => e,
                None => {
                    tracing::error!(module = "Chat", "Event channel closed unexpectedly for conv_id={}", conv_id);
                    if full_text.is_empty() {
                        terminal_event = Some(serde_json::json!({
                            "type": "error",
                            "error": "Connection to engine lost. The engine may have crashed."
                        }));
                    } else {
                        terminal_event = Some(serde_json::json!({
                            "type": "message_stop",
                            "stop_reason": "channel_closed",
                            "full_text": full_text.clone(),
                        }));
                    }
                    break;
                }
            };
            use crate::native_engine::tool_loop::EngineEvent;
            deepseek_code_core::services::tool_history::apply_event(&mut tools,&mut origins,&mut arguments,&event);
            if let EngineEvent::ToolCallRequestLink { tool_use_id, request_id, provider_call_id } = &event {
                request_links.insert(tool_use_id.clone(), (request_id.clone(), Some(provider_call_id.clone())));
            }
            if let EngineEvent::ToolUseDone {tool_use_id,..} | EngineEvent::PipelineToolResult {tool_use_id,..}
                | EngineEvent::NestedToolCall {tool_use_id,..} = &event {
                if let Some(tool)=tools.iter().find(|t| t.id==*tool_use_id).cloned() {
                    let writer=history.clone();
                    let message=assistant_id.clone();
                    let content=full_text.clone();
                    let origin=origins.get(tool_use_id).map(|o|(tool_use_id.clone(),o.clone())).into_iter().collect();
                    let link=request_links.get(tool_use_id).map(|l|(tool_use_id.clone(),l.clone())).into_iter().collect();
                    let saved=tokio::task::spawn_blocking(move || writer.save_segment(&message,SegmentUpdate {
                        text:Some(&content),tools:std::slice::from_ref(&tool),origins:Some(&origin),request_links:Some(&link),..Default::default()
                    })).await;
                    if !matches!(saved,Ok(Ok(()))) {tracing::error!("Could not persist tool result: {saved:?}");}
                }
            }
            let event_data = match event {
                crate::native_engine::tool_loop::EngineEvent::CompactionNotice(message) => {
                    Some(serde_json::json!({"type":"compaction_notice","message":message}))
                }
                crate::native_engine::tool_loop::EngineEvent::ResourceAlert(alert) => {
                    Some(serde_json::json!({
                        "type":"resource_alert",
                        "kind":alert.kind.as_str(),
                        "summary":alert.summary(),
                        "details":alert.details,
                    }))
                }
                crate::native_engine::tool_loop::EngineEvent::ContextReset { session_id, boundary_id, assistant_id: reserved, reason } => {
                    if session_id == conv_id {
                        if let Err(error) = history.save_segment(&assistant_id, SegmentUpdate {
                            placement: SegmentPlacement::Before(&boundary_id), text: Some(&full_text), thinking: Some(""),
                            tools: &tools, origins:Some(&origins), request_links:Some(&request_links), snapshots:Some(&serde_json::json!(request_snapshots)), ..Default::default()
                        }) {
                            terminal_event = Some(serde_json::json!({"type":"error","error":format!("Cannot archive planning context: {error:#}")}));
                            break;
                        }
                        assistant_id = match turn.resume_reset_segment(reserved) {
                            Ok(id) => id,
                            Err(error) => {
                                terminal_event = Some(serde_json::json!({"type":"error","error":format!("Cannot create assistant segment: {error:#}")}));
                                break;
                            }
                        };
                        full_text.clear(); tools.clear(); origins.clear(); request_links.clear(); arguments.clear(); request_snapshots.clear();
                        run.reset_progress();
                        run.set_turn(turn.record(), &assistant_id);
                        if matches!(reason, crate::native_engine::tool_loop::ContextResetReason::PlanApproval) {
                            observed_plan_boundary = Some(boundary_id.clone());
                        }
                    }
                    let content = history.message_text(&boundary_id).ok().flatten().unwrap_or_default();
                    let kind = match reason {
                        crate::native_engine::tool_loop::ContextResetReason::PlanApproval => "plan_approval",
                        crate::native_engine::tool_loop::ContextResetReason::AutoCompact { .. } => "auto_compact",
                    };
                    Some(serde_json::json!({"type":"context_reset","boundary_id":boundary_id,"content":content,"assistant_id":assistant_id,"turn":turn.record(),"reason":kind}))
                }
                crate::native_engine::tool_loop::EngineEvent::RequestSnapshot(mut snapshot) => {
                    if let Some(existing) = request_snapshots.iter_mut().find(|entry| entry["id"] == snapshot["id"]) {
                        snapshot["number"] = existing["number"].clone();
                        *existing = snapshot.clone();
                    } else {
                        request_number += 1;
                        snapshot["number"] = serde_json::json!(request_number);
                        request_snapshots.push(snapshot.clone());
                    }
                    let writer = history.clone();
                    let message_id = assistant_id.clone();
                    let snapshots = serde_json::json!(request_snapshots);
                    let saved = tokio::task::spawn_blocking(move || writer.save_segment(&message_id, SegmentUpdate {
                        snapshots: Some(&snapshots), ..Default::default()
                    })).await;
                    let persisted = matches!(saved, Ok(Ok(())));
                    if !persisted { tracing::error!("Could not persist trajectory request: {:?}", saved); }
                    Some(serde_json::json!({"type": "request_snapshot", "snapshot": snapshot, "persisted": persisted}))
                }
                crate::native_engine::tool_loop::EngineEvent::MessageStart { model } => {
                    Some(serde_json::json!({
                        "type": "message_start",
                        "assistant_id": assistant_id,
                        "turn": turn.record(),
                        "model": model,
                    }))
                }
                crate::native_engine::tool_loop::EngineEvent::QueuedInput(input) => {
                    let queue_item = input.origin.strip_prefix("queue-input-")
                        .and_then(|id| id.parse::<i64>().ok())
                        .and_then(|id| deepseek_code_core::services::input_queue::list(&db_clone, &conv_id).ok()?.into_iter().find(|item| item.id == id));
                    if let Some(tool) = tools.iter().rev().find(|tool|tool.output.is_some()).filter(|_| queue_item.is_none()) {
                        if let Err(error)=history.add_segment_input(&assistant_id,&tool.id,&input) {
                            tracing::error!("Could not persist queued input: {error:#}");
                        }
                    }
                    Some(serde_json::json!({"type": "queued_input", "text": input.text, "content": input.content, "origin": input.origin, "queueItem": queue_item}))
                }
                crate::native_engine::tool_loop::EngineEvent::Text(text) => {
                    full_text.push_str(&text);
                    run.push_text(&text);
                    Some(serde_json::json!({
                        "type": "content_block_delta",
                        "delta": {"type": "text_delta", "text": text},
                    }))
                }
                crate::native_engine::tool_loop::EngineEvent::Thinking(thinking) => {
                    run.push_thinking(&thinking);
                    Some(serde_json::json!({
                        "type": "thinking",
                        "thinking": thinking,
                    }))
                }
                crate::native_engine::tool_loop::EngineEvent::NestedToolCall { tool_use_id, parent_tool_use_id, sequence } => {
                    // Engine-only increment: forwarded for hosts that record
                    // nested history; not a provider protocol item.
                    Some(serde_json::json!({
                        "type": "nested_tool_call",
                        "tool_use_id": tool_use_id,
                        "parent_tool_use_id": parent_tool_use_id,
                        "sequence": sequence,
                    }))
                }
                crate::native_engine::tool_loop::EngineEvent::ToolUseStart { tool_use_id, tool_name, tool_input, text_before, display_title } => {
                    tracing::info!(module = "Chat", "Tool use started: {} ({})", tool_name, tool_use_id);
                    Some(serde_json::json!({
                        "type": "tool_use_start",
                        "tool_use_id": tool_use_id,
                        "tool_name": tool_name,
                        "tool_input": tool_input,
                        "textBefore": text_before,
                        "display_title": display_title,
                    }))
                }
                crate::native_engine::tool_loop::EngineEvent::ToolArgDelta { tool_use_id, delta } => {
                    Some(serde_json::json!({
                        "type": "tool_arg_delta",
                        "tool_use_id": tool_use_id,
                        "delta": delta,
                    }))
                }
                crate::native_engine::tool_loop::EngineEvent::ToolUseDone { tool_use_id, tool_name, tool_input, output, is_error } => {
                    tracing::info!(module = "Chat", "Tool use completed: {} ({}) is_error={}", tool_name, tool_use_id, is_error);
                    Some(serde_json::json!({
                        "type": "tool_use_done",
                        "tool_use_id": tool_use_id,
                        "tool_name": tool_name,
                        "tool_input": tool_input,
                        "output": output,
                        "content": output,
                        "is_error": is_error,
                    }))
                }
                crate::native_engine::tool_loop::EngineEvent::MessageDelta { stop_reason } => {
                    Some(serde_json::json!({
                        "type": "message_delta",
                        "delta": {"stop_reason": stop_reason},
                    }))
                }
                crate::native_engine::tool_loop::EngineEvent::MessageStop { full_text: _, stop_reason } => {
                    terminal_event = Some(serde_json::json!({
                        "type": "message_stop", "stop_reason": stop_reason, "full_text": full_text.clone(),
                    }));
                    break;
                }
                crate::native_engine::tool_loop::EngineEvent::Error(err) => {
                    tracing::error!(module = "Chat", "Engine error: {}", err);
                    Some(serde_json::json!({
                        "type": "error",
                        "error": err,
                    }))
                }
                crate::native_engine::tool_loop::EngineEvent::Usage(usage) => {
                    Some(serde_json::json!({
                        "type": "usage",
                        "usage": usage,
                    }))
                }
                crate::native_engine::tool_loop::EngineEvent::AskUser { request_id, tool_use_id, tool_input, question, options } => {
                    let source = deepseek_code_core::native_engine::permission_response::request_owner(&request_id)
                        .map(|owner| owner.session_id).unwrap_or_else(|| conv_id.clone());
                    let options_json: Vec<serde_json::Value> = options.iter()
                        .map(|o| serde_json::json!({"label": o, "description": ""}))
                        .collect();
                    Some(serde_json::json!({
                        "type": "ask_user",
                        "request_id": request_id,
                        "tool_use_id": tool_use_id,
                        "session_id": source,
                        "tool_input": tool_input,
                        "planApprovalRequest": tool_input.get("planApprovalRequest"),
                        "questions": [{
                            "question": question,
                            "options": options_json
                        }],
                    }))
                }
                crate::native_engine::tool_loop::EngineEvent::ToolPermission { request_id, tool_use_id, tool_name, input } => {
                    let closed = match native_engine.lock().await.as_ref() {
                        Some(engine) => engine.permission_completion(&request_id).await,
                        None => tokio::sync::watch::channel(true).1,
                    };
                    if !permission_watches.register(&request_id, &tool_use_id, closed) {
                        yield Ok::<Event, Infallible>(Event::default().data(super::permission_watch::resolved(&conv_id, &request_id, &tool_use_id).to_string()));
                        continue;
                    }
                    Some(serde_json::json!({
                        "type": "tool_permission",
                        "toolscript": deepseek_code_core::native_engine::permission_response::toolscript_origin(&request_id),
                        "session_id": deepseek_code_core::native_engine::permission_response::request_owner(&request_id).map(|owner| owner.session_id).unwrap_or_else(|| conv_id.clone()),
                        "request_id": request_id,
                        "tool_use_id": tool_use_id,
                        "allow_session": deepseek_code_core::native_engine::permission_response::can_allow_bash_session(&tool_name, &input),
                        "tool_name": tool_name,
                        "input": input,
                    }))
                }
                crate::native_engine::tool_loop::EngineEvent::PipelineToolResult { tool_use_id, tool_name, tool_input, output, is_error, completed_count, total_count } => {
                    Some(serde_json::json!({
                        "type": "pipeline_tool_result",
                        "tool_use_id": tool_use_id,
                        "tool_name": tool_name,
                        "tool_input": tool_input,
                        "output": output,
                        "is_error": is_error,
                        "completed_count": completed_count,
                        "total_count": total_count,
                    }))
                }
                crate::native_engine::tool_loop::EngineEvent::AgentProgress { session_id, agent_type, status, tool_name, .. } => {
                    Some(serde_json::json!({
                        "type": "agent_progress",
                        "session_id": session_id,
                        "agent_type": agent_type,
                        "status": status,
                        "tool_name": tool_name,
                    }))
                }
                crate::native_engine::tool_loop::EngineEvent::RequestAttempt(event) => Some(event),
                // Consumed into request_links above; it is not a client event.
                crate::native_engine::tool_loop::EngineEvent::ToolCallRequestLink { .. } => None,
                crate::native_engine::tool_loop::EngineEvent::BudgetWarning { message, usage, limit } => {
                    Some(serde_json::json!({
                        "type": "budget_warning",
                        "message": message,
                        "usage": usage,
                        "limit": limit,
                    }))
                }
            };

            if let Some(data) = event_data {
                let is_stop = data.get("type").and_then(|t| t.as_str()) == Some("message_stop")
                    || data.get("type").and_then(|t| t.as_str()) == Some("error");
                if is_stop { terminal_event = Some(data); break; }
                run.record_event();
                let data = if h5 {
                    let Some(event) = super::h5::client_event(data) else {continue};
                    event
                } else {data};
                yield Ok::<Event, Infallible>(Event::default().data(data.to_string()));
            }
        }

        // Closing this turn's receiver cancels its executor without addressing a newer turn.
        drop(rx);
        tracing::info!(module = "Chat", "Stream ended for conv_id={}", conv_id);

        crate::metrics::ACTIVE_SSE_CONNECTIONS.dec();

        let status = deepseek_code_core::db::turn_repo::terminal_status(
            terminal_event.as_ref().and_then(|event| event["stop_reason"].as_str()),
            terminal_event.as_ref().is_none_or(|event| event["type"] == "error" || event.get("error").is_some()),
        );
        let final_turn = match turn.finish(status) {
            Ok(record) => Some(record),
            Err(error) => { tracing::error!("Could not finalize turn: {error:#}"); None }
        };
        let final_assistant_id = assistant_id.clone();

        // A cancelled/crashed engine may never emit its final RequestSnapshot.
        // Persist a terminal status without inventing missing usage or timings.
        let mut pending_snapshot_updates = Vec::new();
        for snapshot in &mut request_snapshots {
            if snapshot["status"] == "running" {
                let error = terminal_event.as_ref().and_then(|event| event.get("error")).cloned();
                snapshot["status"] = serde_json::json!(if error.is_some() { "error" } else { "interrupted" });
                snapshot["endedAt"] = serde_json::json!(chrono::Utc::now().timestamp_millis());
                if let Some(error) = error { snapshot["error"] = error; }
                pending_snapshot_updates.push(snapshot.clone());
            }
        }

        for resolved in permission_watches.finish(&conv_id) {
            yield Ok::<Event, Infallible>(Event::default().data(resolved.to_string()));
        }
        // Save assistant message to SQLite after stream ends
        {
            let saved = tokio::task::spawn_blocking(move || {
                history.save_segment(&assistant_id,SegmentUpdate {
                    placement: SegmentPlacement::Final {observed_boundary:observed_plan_boundary.as_deref()},
                    text: Some(&full_text),tools:&tools,origins:Some(&origins),request_links:Some(&request_links),snapshots:Some(&serde_json::json!(request_snapshots)),..Default::default()
                })
            }).await;
            let persisted=matches!(saved,Ok(Ok(())));
            if !persisted {
                tracing::error!("Could not persist final assistant trajectory: {:?}", saved);
                let _ = deepseek_code_core::services::input_queue::set_paused(&lifecycle_state.db_manager, &lifecycle_id, true);
            }
            for snapshot in pending_snapshot_updates.into_iter().filter(|_|!h5) {
                yield Ok::<Event, Infallible>(Event::default().data(serde_json::json!({"type":"request_snapshot", "snapshot":snapshot, "persisted":persisted}).to_string()));
            }
        }
        if let Some(record) = final_turn.filter(|_|!h5) {
            yield Ok::<Event, Infallible>(Event::default().data(serde_json::json!({"type":"turn_timing","turn":record,"assistant_id":final_assistant_id}).to_string()));
        }
        if terminal_event.as_ref().is_none_or(|event| event["type"] == "error" || event["stop_reason"] == "cancelled" || event["stop_reason"] == "error" || event["stop_reason"] == "channel_closed") {
            let _ = deepseek_code_core::services::input_queue::set_paused(&lifecycle_state.db_manager, &lifecycle_id, true);
        }
        // Report idle only once the trajectory above is persisted.
        drop(run);
        drop(_turn_lease);
        super::input_queue::kick(lifecycle_state, lifecycle_id);
        if let Some(event) = terminal_event {
            let event = if h5 && event["type"]=="message_stop" {serde_json::json!({"type":"message_stop","stop_reason":event["stop_reason"]})}else{event};
            yield Ok::<Event, Infallible>(Event::default().data(event.to_string()));
        }
    };

    // Collection and durable completion continue after an observer disconnects.
    // Explicit Stop remains the only transport-independent cancellation action.
    tokio::spawn(async move {
        use futures::StreamExt;
        futures::pin_mut!(stream);
        while let Some(event) = stream.next().await {
            if sse_tx.send(event).await.is_err() && h5 { break; }
        }
    });
    let mut response = Sse::new(tokio_stream::wrappers::ReceiverStream::new(sse_rx)).keep_alive(KeepAlive::default()).into_response();
    set_sse_content_type(&mut response);
    response
}

async fn chat_stream_handler(
    State(state): State<AppState>,
    Query(query): Query<StreamQuery>,
) -> Result<impl IntoResponse, StatusCode> {
    let stream_manager = state.stream_manager.clone();
    let mut manager: tokio::sync::MutexGuard<'_, StreamManager> = stream_manager.lock().await;

    let receiver = manager.add_listener(&query.conversation_id)
        .ok_or_else(|| StatusCode::NOT_FOUND)?;

    let idle_timeout = Duration::from_secs(SSE_IDLE_TIMEOUT_SECS);
    let max_duration = Duration::from_secs(SSE_MAX_DURATION_SECS);
    let mut last_activity = std::time::Instant::now();

    crate::metrics::ACTIVE_SSE_CONNECTIONS.inc();
    let stream = async_stream::stream! {
        let mut rx = receiver;
        let max_sleep = tokio::time::sleep(max_duration);
        tokio::pin!(max_sleep);

        loop {
            let idle_sleep = tokio::time::sleep(idle_timeout);
            tokio::pin!(idle_sleep);

            tokio::select! {
                _ = &mut max_sleep => {
                    tracing::error!(module = "SSE_chat_stream", "Max duration reached, closing stream");
                    break;
                }
                _ = &mut idle_sleep => {
                    if last_activity.elapsed() >= idle_timeout {
                        tracing::error!(module = "SSE_chat_stream", "Idle timeout, closing stream");
                        break;
                    }
                }
                result = rx.recv() => {
                    match result {
                        Ok(event) => {
                            last_activity = std::time::Instant::now();
                            let event_name = event.event_type;
                            let data = serde_json::to_string(&event.data).unwrap_or_default();
                            yield Ok::<Event, Infallible>(Event::default()
                                .event(&event_name)
                                .data(data));
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            tracing::error!(module = "SSE_chat_stream", "Receiver lagged, dropped {} events", n);
                            last_activity = std::time::Instant::now();
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            break;
                        }
                    }
                }
            }
        }

        tracing::info!(module = "SSE_chat_stream", "Stream ended for conv_id={}", query.conversation_id);
        crate::metrics::ACTIVE_SSE_CONNECTIONS.dec();
    };

    let mut response = Sse::new(stream).keep_alive(KeepAlive::default()).into_response();
    set_sse_content_type(&mut response);
    Ok(response)
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/chat", post(chat_handler))
        .route("/api/chat/stream", get(chat_stream_handler))
}
